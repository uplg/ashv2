use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;

use log::{debug, error, info, trace, warn};
use tokio::io::AsyncRead;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::SendError;

use self::buffer::Buffer;
use crate::actor::message::Message;
use crate::frame::{Ack, Data, Error, Frame, Nak, Rst, RstAck};
use crate::protocol::{Mask, next_sequence_number};
use crate::types::{MAX_FRAME_SIZE, Payload};
use crate::validate::Validate;

mod buffer;

/// Expected frame number before the first DATA frame arrives.
const INITIAL_ACK_NUMBER: u8 = 0;

/// `ASHv2` receiver.
#[derive(Debug)]
pub struct Receiver<R> {
    buffer: Buffer<R>,
    response: Sender<Payload>,
    transmitter: Sender<Message>,
    last_received_frame_num: Option<u8>,
}

impl<R> Receiver<R>
where
    R: AsyncRead,
{
    /// Creates a new `ASHv2` receiver.
    pub fn new(reader: R, response: Sender<Payload>, transmitter: Sender<Message>) -> Self {
        Self {
            buffer: Buffer::new(reader),
            response,
            transmitter,
            last_received_frame_num: None,
        }
    }
}

impl<R> Receiver<R>
where
    R: AsyncRead + Sync + Unpin,
{
    /// Runs the receiver loop.
    pub async fn run(mut self, running: Arc<AtomicBool>) {
        trace!("Starting receiver with frame size: {MAX_FRAME_SIZE}");

        while running.load(Relaxed) {
            let frame = match self.buffer.read_frame().await {
                Ok(Ok(frame)) => frame,
                Ok(Err(error)) => {
                    warn!("Discarding invalid frame: {error}");
                    continue;
                }
                Err(error) => {
                    // After an I/O error or the end of the stream, the reader stays exhausted:
                    // retrying would busy-spin on the same error forever without ever yielding
                    // to the runtime. Exit instead, so that the caller can observe the
                    // terminated receiver future and rebuild the transport.
                    error!("Fatal error receiving frame, receiver exiting: {error}");
                    break;
                }
            };

            trace!("Received frame: {frame:#04X}");

            if let Err(error) = self.handle_frame(frame).await {
                info!("Transmitter channel closed, receiver exiting: {error}");
                break;
            }
        }

        debug!("Receiver loop terminated.");
    }

    /// Returns the ACK number.
    ///
    /// This is equal to the last received frame number plus one.
    fn ack_number(&self) -> u8 {
        self.last_received_frame_num
            .map_or(INITIAL_ACK_NUMBER, next_sequence_number)
    }

    async fn handle_frame(&mut self, frame: Frame) -> Result<(), SendError<Message>> {
        match frame {
            Frame::Ack(ack) => self.handle_ack(ack).await,
            Frame::Data(data) => self.handle_data(*data).await,
            Frame::Error(error) => self.handle_error(error).await,
            Frame::Nak(nak) => self.handle_nak(nak).await,
            Frame::Rst(rst) => self.handle_rst(rst).await,
            Frame::RstAck(rst_ack) => self.handle_rst_ack(rst_ack).await,
        }
    }

    /// Handle an incoming `ACK` frame.
    async fn handle_ack(&self, ack: Ack) -> Result<(), SendError<Message>> {
        if let Ok(ack) = ack.validate() {
            self.ack_sent_frames(ack.ack_num()).await
        } else {
            warn!("Received ACK with invalid CRC.");
            Ok(())
        }
    }

    /// Handle an incoming `DATA` frame.
    async fn handle_data(&mut self, data: Data) -> Result<(), SendError<Message>> {
        trace!("Handling data frame: {data:#04X}");

        let Ok(data) = data.validate() else {
            warn!("Received data frame with invalid CRC.");
            self.send_nak().await?;
            return Ok(());
        };

        if data.frame_num() == self.ack_number() {
            trace!("Received in-sequence data frame: {data}");
            self.last_received_frame_num.replace(data.frame_num());
            self.send_ack().await?;
            self.ack_sent_frames(data.ack_num()).await?;
            self.handle_payload(data.into_payload()).await;
            return Ok(());
        }

        if data.is_retransmission() {
            // A retransmission of the frame we expect next was handled as in-sequence above.
            // Any other retransmission repeats a payload that has already been delivered:
            // acknowledge it, but do not forward the payload again, as feeding the same bytes
            // twice to the upper layer desynchronises it.
            debug!("Discarding duplicate retransmission of data frame: {data}");
            self.send_ack().await?;
            self.ack_sent_frames(data.ack_num()).await?;
            return Ok(());
        }

        warn!("Received out-of-sequence data frame: {data}");
        self.send_nak().await?;
        Ok(())
    }

    async fn handle_error(&self, error: Error) -> Result<(), SendError<Message>> {
        if let Ok(error) = error.validate() {
            self.transmitter.send(Message::Error(error)).await
        } else {
            warn!("Received ERROR with invalid CRC.");
            Ok(())
        }
    }

    /// Handle an incoming `NAK` frame.
    async fn handle_nak(&self, nak: Nak) -> Result<(), SendError<Message>> {
        if let Ok(nak) = nak.validate() {
            self.nak_sent_frames(nak.ack_num()).await
        } else {
            warn!("Received NAK with invalid CRC.");
            Ok(())
        }
    }

    async fn handle_rst(&mut self, rst: Rst) -> Result<(), SendError<Message>> {
        if let Ok(rst) = rst.validate() {
            self.restart_frame_numbering();
            self.transmitter.send(Message::Rst(rst)).await
        } else {
            warn!("Received RST with invalid CRC.");
            Ok(())
        }
    }

    async fn handle_rst_ack(&mut self, rst_ack: RstAck) -> Result<(), SendError<Message>> {
        if let Ok(rst_ack) = rst_ack.validate() {
            self.restart_frame_numbering();
            self.transmitter.send(Message::RstAck(rst_ack)).await
        } else {
            warn!("Received RST-ACK with invalid CRC.");
            Ok(())
        }
    }

    /// Restart the expected frame numbering after a reset.
    ///
    /// Per the ASH specification, frame numbering restarts from zero on both sides after a
    /// reset. Without this, every post-reset `DATA` frame from the NCP would be considered
    /// out of sequence and `NAK`ed, wedging the link.
    const fn restart_frame_numbering(&mut self) {
        self.last_received_frame_num = None;
    }

    /// Send the response frame's payload through the response channel.
    async fn handle_payload(&self, mut payload: Payload) {
        payload.mask();
        self.response.send(payload).await.unwrap_or_else(|error| {
            error!("Failed to send payload through response channel: {error}");
        });
    }

    /// Send an `ACK` frame.
    async fn send_ack(&self) -> Result<(), SendError<Message>> {
        self.transmitter
            .send(Message::SendAck(self.ack_number()))
            .await
    }

    /// Send a `NAK` frame.
    async fn send_nak(&self) -> Result<(), SendError<Message>> {
        self.transmitter
            .send(Message::SendNak(self.ack_number()))
            .await
    }

    /// Acknowledge sent frames up to `ack_num`.
    async fn ack_sent_frames(&self, ack_num: u8) -> Result<(), SendError<Message>> {
        self.transmitter.send(Message::ReceivedAck(ack_num)).await
    }

    /// Negative acknowledge sent frames up to `ack_num`.
    async fn nak_sent_frames(&self, ack_num: u8) -> Result<(), SendError<Message>> {
        self.transmitter.send(Message::ReceivedNak(ack_num)).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use tokio::io::{Empty, empty};
    use tokio::runtime::Builder;
    use tokio::sync::mpsc::{Receiver as MpscReceiver, channel};

    use super::Receiver;
    use crate::actor::message::Message;
    use crate::frame::{Data, RstAck};
    use crate::types::Payload;

    const RST_ACK_BYTES: [u8; 5] = [0xC1, 0x02, 0x02, 0x9B, 0x7B];
    const CHANNEL_SIZE: usize = 8;

    #[expect(
        clippy::iter_with_drain,
        reason = "RstAck is parsed from a drained buffer"
    )]
    fn rst_ack() -> RstAck {
        let mut bytes = RST_ACK_BYTES.to_vec();
        RstAck::try_from(bytes.drain(..).peekable()).expect("reference RSTACK should parse")
    }

    fn receiver() -> (
        Receiver<Empty>,
        MpscReceiver<Payload>,
        MpscReceiver<Message>,
    ) {
        let (response_tx, response_rx) = channel(CHANNEL_SIZE);
        let (transmitter_tx, transmitter_rx) = channel(CHANNEL_SIZE);
        (
            Receiver::new(empty(), response_tx, transmitter_tx),
            response_rx,
            transmitter_rx,
        )
    }

    #[test]
    fn exits_on_end_of_stream_but_skips_invalid_frames() {
        Builder::new_current_thread()
            .build()
            .expect("runtime should build")
            .block_on(async {
                const FLAG: u8 = 0x7E;
                // A truncated DATA frame, then a valid RSTACK, then the end of the stream.
                let mut input = vec![0x01, FLAG];
                input.extend(RST_ACK_BYTES);
                input.push(FLAG);
                let (response_tx, _responses) = channel(CHANNEL_SIZE);
                let (transmitter_tx, mut messages) = channel(CHANNEL_SIZE);
                let receiver = Receiver::new(Cursor::new(input), response_tx, transmitter_tx);

                // Returns instead of spinning forever on the exhausted stream.
                receiver.run(Arc::new(AtomicBool::new(true))).await;

                assert!(matches!(messages.try_recv(), Ok(Message::RstAck(_))));
            });
    }

    #[test]
    fn acks_but_does_not_forward_duplicate_retransmissions() {
        Builder::new_current_thread()
            .build()
            .expect("runtime should build")
            .block_on(async {
                let (mut receiver, mut responses, mut messages) = receiver();
                let payload: Payload = [0x01, 0x02, 0x03].into_iter().collect();
                let data = Data::new(0, 0, payload.clone());
                let mut retransmission = data.clone();
                retransmission.set_is_retransmission(true);

                receiver
                    .handle_data(data)
                    .await
                    .expect("DATA should be handled");
                assert_eq!(responses.try_recv().ok(), Some(payload));
                assert!(matches!(messages.try_recv(), Ok(Message::SendAck(1))));
                assert!(matches!(messages.try_recv(), Ok(Message::ReceivedAck(0))));

                receiver
                    .handle_data(retransmission)
                    .await
                    .expect("retransmitted DATA should be handled");
                assert!(responses.try_recv().is_err());
                assert!(matches!(messages.try_recv(), Ok(Message::SendAck(1))));
                assert!(matches!(messages.try_recv(), Ok(Message::ReceivedAck(0))));
            });
    }

    #[test]
    fn restarts_frame_numbering_after_rst_ack() {
        Builder::new_current_thread()
            .build()
            .expect("runtime should build")
            .block_on(async {
                let (mut receiver, _responses, mut messages) = receiver();
                receiver.last_received_frame_num = Some(3);
                assert_eq!(receiver.ack_number(), 4);

                receiver
                    .handle_rst_ack(rst_ack())
                    .await
                    .expect("RSTACK should be forwarded");

                assert_eq!(receiver.ack_number(), 0);
                assert!(matches!(messages.try_recv(), Ok(Message::RstAck(_))));
            });
    }
}
