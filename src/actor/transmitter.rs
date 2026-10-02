use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind;
use std::ops::BitAnd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use log::{debug, error, info, trace, warn};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tokio::time::timeout;

use self::buffer::Buffer;
use self::transmission::Transmission;
use crate::actor::message::Message;
use crate::frame::{Ack, Data, Error, Nak, RST, Rst, RstAck};
use crate::hex_slice::HexSlice;
use crate::protocol::next_sequence_number;
use crate::status::Status;
use crate::types::{MAX_FRAME_SIZE, Payload};
use crate::{SEQ_MASK, T_RSTACK_MAX_MILLIS, T_RX_ACK_MAX_MILLIS, TX_K};

mod buffer;
mod transmission;

/// Maximum time to wait for RST ACK frame after sending RST frame.
const T_RSTACK_MAX: Duration = Duration::from_millis(T_RSTACK_MAX_MILLIS);

const T_RX_ACK_MAX: Duration = Duration::from_millis(T_RX_ACK_MAX_MILLIS);

/// Delay between housekeeping ticks while the transmitter has a backlog.
const TICK_DELAY: Duration = Duration::from_millis(100);

/// Maximum number of payloads held in the local pending queue.
///
/// The pending queue holds payloads that cannot be transmitted yet, because the link is down or
/// the transmission window is full. It is local to the transmitter on purpose: the transmitter
/// must never send into its own bounded input channel, since it is the only consumer of that
/// channel and would deadlock as soon as the channel is full.
const PENDING_CAPACITY: usize = 64;

/// A payload waiting for a free transmission slot, with its caller's response channel.
type PendingPayload = (Box<Payload>, oneshot::Sender<io::Result<()>>);

/// `ASHv2` transmitter.
#[derive(Debug)]
pub struct Transmitter<T> {
    buffer: Buffer<T>,
    messages: Receiver<Message>,
    pending: VecDeque<PendingPayload>,
    status: Status,
    last_rst_sent: Option<Instant>,
    transmissions: heapless::Vec<Transmission, TX_K>,
    frame_number: u8,
    ack_number: u8,
}

impl<T> Transmitter<T> {
    /// Creates a new `ASHv2` transmitter.
    #[must_use]
    pub const fn new(writer: T, messages: Receiver<Message>) -> Self {
        Self {
            buffer: Buffer::new(writer),
            messages,
            pending: VecDeque::new(),
            status: Status::Uninitialized,
            last_rst_sent: None,
            transmissions: heapless::Vec::new(),
            frame_number: 0,
            ack_number: 0,
        }
    }
}

impl<T> Transmitter<T>
where
    T: AsyncWrite + Sync + Unpin,
{
    /// Runs the transmitter, processing messages from the channel.
    pub async fn run(mut self, running: Arc<AtomicBool>) {
        trace!("Starting transmitter with frame size: {MAX_FRAME_SIZE}");
        self.reset().await.unwrap_or_else(|error| {
            error!("Failed to send initial RST frame: {error}");
        });

        loop {
            let result = if self.has_backlog() {
                match timeout(TICK_DELAY, self.messages.recv()).await {
                    Ok(Some(message)) => self.handle_message(message).await,
                    Ok(None) => break,
                    Err(_) => self.tick().await,
                }
            } else {
                let Some(message) = self.messages.recv().await else {
                    break;
                };
                self.handle_message(message).await
            };

            if let Err(error) = result {
                error!("Resetting connection due to I/O error: {error}");
                self.status = Status::Failed;
            }
        }

        running.store(false, Relaxed);
        info!("Transmitter loop terminated.");
    }

    /// Returns `true` if there is work that requires periodic housekeeping.
    fn has_backlog(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Periodic housekeeping while there is a backlog.
    ///
    /// Keeps trying to (re-)establish the connection while payloads are pending, and transmits
    /// pending payloads once there is room in the transmission window.
    async fn tick(&mut self) -> io::Result<()> {
        if self.status != Status::Connected {
            return self.reset().await;
        }

        self.flush_pending().await
    }

    async fn handle_message(&mut self, message: Message) -> io::Result<()> {
        trace!("Received message: {message}");

        if self.status != Status::Connected {
            match message {
                Message::RstAck(ack) => return self.handle_rst_ack(ack).await,
                // Keep payloads for delivery after the connection has been established.
                Message::Payload {
                    payload,
                    response_tx,
                } => self.enqueue_pending(payload, response_tx),
                // Link-control messages refer to the frame space of the previous session.
                // Replaying them after reconnection would only corrupt the new session.
                other => trace!("Dropping link-control message while disconnected: {other}"),
            }

            // Only log if the connection has failed, not if it hasn't been established yet.
            if self.status == Status::Failed {
                warn!("ASHv2 Connection failed. Resetting...");
            }

            return self.reset().await;
        }

        match message {
            Message::Payload {
                payload,
                response_tx,
            } => {
                // Always go through the pending queue to preserve the payloads' order.
                self.enqueue_pending(payload, response_tx);
                self.flush_pending().await
            }
            Message::SendAck(ack_num) => self.send_ack(ack_num).await,
            Message::SendNak(ack_num) => self.send_nak(ack_num).await,
            Message::Rst(rst) => self.handle_rst(rst).await,
            Message::RstAck(rst_ack) => self.handle_rst_ack(rst_ack).await,
            Message::Error(error) => self.handle_error(error).await,
            Message::ReceivedAck(ack_num) => {
                self.ack_sent_frames(ack_num);
                self.flush_pending().await
            }
            Message::ReceivedNak(ack_num) => self.nak_sent_frames(ack_num).await,
        }
    }

    /// Transmit pending payloads while connected and the transmission window has room.
    async fn flush_pending(&mut self) -> io::Result<()> {
        while self.status == Status::Connected
            && !self.transmissions.is_full()
            && let Some((payload, response)) = self.pending.pop_front()
        {
            self.send_payload(payload, response).await?;
        }

        if !self.pending.is_empty() {
            trace!("{} payload(s) pending transmission.", self.pending.len());
        }

        Ok(())
    }

    /// Transmit a payload as a new `DATA` frame.
    ///
    /// The caller must make sure that there is room in the transmission window.
    async fn send_payload(
        &mut self,
        payload: Box<Payload>,
        response: oneshot::Sender<io::Result<()>>,
    ) -> io::Result<()> {
        let data = Data::new(self.next_frame_number(), self.ack_number, *payload);
        // With a sliding windows size > 1 the NCP may enter an "ERROR: Assert" state when sending
        // fragmented messages if each DATA frame's ACK number is not increased.
        self.ack_number = next_sequence_number(self.ack_number);
        response
            .send(self.transmit(data.into()).await)
            .unwrap_or_else(|_| {
                error!("Failed to send transmit result through response channel.");
            });
        Ok(())
    }

    async fn send_ack(&mut self, ack_num: u8) -> io::Result<()> {
        self.ack_number = ack_num;
        self.buffer.write_frame(Ack::new(ack_num, false)).await
    }

    async fn send_nak(&mut self, ack_num: u8) -> io::Result<()> {
        self.buffer.write_frame(Nak::new(ack_num, false)).await
    }

    /// Handle RST frame received from the NCP.
    async fn handle_rst(&mut self, rst: Rst) -> io::Result<()> {
        error!("Received RST frame: {rst}, resetting connection.");
        self.status = Status::Failed;
        self.reset().await
    }

    /// Handle RST ACK frame received from the NCP.
    async fn handle_rst_ack(&mut self, rst_ack: RstAck) -> io::Result<()> {
        trace!("Received RST ACK frame: {rst_ack}, connection reset acknowledged.");

        if !rst_ack.is_ash_v2() {
            error!("Received RST ACK frame with invalid ASH version: {rst_ack}.");
            return Ok(());
        }

        if let Some(timestamp) = self.last_rst_sent.take() {
            if timestamp.elapsed() < T_RSTACK_MAX {
                debug!("Connection established successfully.");
                self.status = Status::Connected;
                // Per the ASH specification, both sides restart frame numbering from zero after
                // a reset. Frames in flight belong to the previous session and are lost; their
                // callers have already been answered and the upper layer retries on timeout.
                self.frame_number = 0;
                self.ack_number = 0;
                self.transmissions.clear();
                self.flush_pending().await
            } else {
                warn!("RST ACK received after timeout. Resetting connection again.");
                self.reset().await
            }
        } else {
            warn!("Received unexpected RST ACK frame: {rst_ack}.");
            Ok(())
        }
    }

    /// Handle errors received from the NCP.
    async fn handle_error(&mut self, error: Error) -> io::Result<()> {
        warn!("Transmitter encountered error: {error}, resetting connection.");
        self.status = Status::Failed;
        self.reset().await
    }

    /// Remove `DATA` frames from the queue that have been acknowledged by the NCP.
    fn ack_sent_frames(&mut self, ack_num: u8) {
        self.remove_timed_out_transmissions();

        // Remove acknowledged transmissions.
        while let Some(transmission) = self
            .transmissions
            .iter()
            .position(|transmission| {
                next_sequence_number(transmission.frame_num()) == ack_num.bitand(SEQ_MASK)
            })
            .map(|index| self.transmissions.remove(index))
        {
            trace!(
                "ACKed frame {transmission} after {:?}",
                transmission.elapsed()
            );
        }
    }

    /// Retransmit `DATA` frames that have been `NAK`ed by the NCP.
    async fn nak_sent_frames(&mut self, nak_num: u8) -> io::Result<()> {
        self.remove_timed_out_transmissions();

        // Retransmit NAK'ed transmission.
        if let Some(transmission) = self
            .transmissions
            .iter()
            .position(|transmission| transmission.frame_num() == nak_num)
            .map(|index| self.transmissions.remove(index))
        {
            debug!("Retransmitting NAK'ed frame #{}", transmission.frame_num());
            self.transmit(transmission).await?;
        }

        Ok(())
    }

    /// Removes transmissions that have exceeded the acknowledgement timeout.
    fn remove_timed_out_transmissions(&mut self) {
        self.transmissions
            .retain(|transmission| !transmission.is_timed_out(T_RX_ACK_MAX));
    }

    /// Send a `DATA` frame.
    async fn transmit(&mut self, mut transmission: Transmission) -> io::Result<()> {
        let data = transmission.data_for_transmit()?;
        trace!("Transmitting frame {data:#04X}");
        self.buffer.write_frame(data).await?;
        self.transmissions
            .insert(0, transmission)
            .map_err(|_| io::Error::new(ErrorKind::OutOfMemory, "Failed to enqueue retransmit"))
    }

    /// Send RST frame to reset the connection.
    async fn reset(&mut self) -> io::Result<()> {
        if let Some(timestamp) = self.last_rst_sent.take()
            && timestamp.elapsed() < T_RSTACK_MAX
        {
            debug!("Last RST sent {timestamp:?} ago, waiting before sending another...");
            self.last_rst_sent.replace(timestamp);
            return Ok(());
        }

        self.last_rst_sent.replace(Instant::now());
        self.buffer.write_frame(RST).await
    }

    /// Returns the next frame number.
    pub const fn next_frame_number(&mut self) -> u8 {
        let frame_number = self.frame_number;
        self.frame_number = next_sequence_number(self.frame_number);
        frame_number
    }

    /// Queue a payload locally until it can be transmitted.
    ///
    /// If the queue is full, the oldest payload is dropped and its caller is notified.
    fn enqueue_pending(
        &mut self,
        payload: Box<Payload>,
        response: oneshot::Sender<io::Result<()>>,
    ) {
        if self.pending.len() >= PENDING_CAPACITY
            && let Some((dropped, response)) = self.pending.pop_front()
        {
            warn!(
                "Pending queue full, dropping oldest payload: {:#04X}",
                HexSlice::new(&dropped)
            );
            response
                .send(Err(io::Error::new(
                    ErrorKind::OutOfMemory,
                    "ASHv2 pending queue is full",
                )))
                .unwrap_or_else(|_| {
                    error!("Failed to send transmit result through response channel.");
                });
        }

        self.pending.push_back((payload, response));
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::{self, ErrorKind};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use tokio::io::{Sink, sink};
    use tokio::runtime::Builder;
    use tokio::sync::mpsc::channel;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::{PENDING_CAPACITY, Transmitter};
    use crate::actor::message::Message;
    use crate::frame::RstAck;
    use crate::status::Status;
    use crate::types::Payload;

    const RST_ACK_BYTES: [u8; 5] = [0xC1, 0x02, 0x02, 0x9B, 0x7B];
    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    fn block_on<F: Future>(future: F) -> F::Output {
        Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime should build")
            .block_on(future)
    }

    #[expect(
        clippy::iter_with_drain,
        reason = "RstAck is parsed from a drained buffer"
    )]
    fn rst_ack() -> RstAck {
        let mut bytes = RST_ACK_BYTES.to_vec();
        RstAck::try_from(bytes.drain(..).peekable()).expect("reference RSTACK should parse")
    }

    fn transmitter() -> Transmitter<Sink> {
        let (_sender, inbox) = channel(1);
        Transmitter::new(sink(), inbox)
    }

    fn payload() -> (Message, oneshot::Receiver<io::Result<()>>) {
        let (response_tx, response_rx) = oneshot::channel();
        let message = Message::Payload {
            payload: Box::new(Payload::new()),
            response_tx,
        };
        (message, response_rx)
    }

    /// Connect the transmitter as if the NCP had answered our RST.
    async fn connect(transmitter: &mut Transmitter<Sink>) {
        transmitter.reset().await.expect("RST should be written");
        transmitter
            .handle_message(Message::RstAck(rst_ack()))
            .await
            .expect("RSTACK should be handled");
        assert_eq!(transmitter.status, Status::Connected);
    }

    #[test]
    fn keeps_draining_inbox_while_disconnected() {
        block_on(async {
            // A capacity of one is the worst case for a transmitter that re-queues into its own
            // inbox: it would block on its own full channel and never receive again.
            let (sender, inbox) = channel(1);
            let running = Arc::new(AtomicBool::new(true));
            let task = tokio::spawn(Transmitter::new(sink(), inbox).run(running));

            for _ in 0..2 * PENDING_CAPACITY {
                let (message, _response) = payload();
                timeout(TEST_TIMEOUT, sender.send(message))
                    .await
                    .expect("transmitter must keep draining its inbox")
                    .expect("transmitter inbox should be open");
            }

            drop(sender);
            timeout(TEST_TIMEOUT, task)
                .await
                .expect("transmitter should terminate")
                .expect("transmitter should not panic");
        });
    }

    #[test]
    fn queues_payloads_until_connected() {
        block_on(async {
            let mut transmitter = transmitter();
            let (message, mut response) = payload();
            transmitter
                .handle_message(message)
                .await
                .expect("payload should be queued");
            assert_eq!(transmitter.pending.len(), 1);
            assert!(response.try_recv().is_err());

            connect(&mut transmitter).await;
            assert!(transmitter.pending.is_empty());
            assert_eq!(transmitter.transmissions.len(), 1);
            assert!(matches!(response.try_recv(), Ok(Ok(()))));
        });
    }

    #[test]
    fn restarts_frame_numbering_after_reset() {
        block_on(async {
            let mut transmitter = transmitter();
            connect(&mut transmitter).await;

            for _ in 0..3 {
                let (message, _response) = payload();
                transmitter
                    .handle_message(message)
                    .await
                    .expect("payload should be sent");
            }
            assert_eq!(transmitter.frame_number, 3);
            assert_eq!(transmitter.transmissions.len(), 3);

            transmitter.status = Status::Failed;
            connect(&mut transmitter).await;
            assert_eq!(transmitter.frame_number, 0);
            assert_eq!(transmitter.ack_number, 0);
            assert!(transmitter.transmissions.is_empty());
        });
    }

    #[test]
    fn drops_link_control_messages_while_disconnected() {
        block_on(async {
            let mut transmitter = transmitter();
            transmitter
                .handle_message(Message::SendAck(3))
                .await
                .expect("message should be handled");
            assert!(transmitter.pending.is_empty());
        });
    }

    #[test]
    fn evicts_oldest_pending_payload_when_full() {
        block_on(async {
            let mut transmitter = transmitter();
            let mut responses = Vec::new();

            for _ in 0..=PENDING_CAPACITY {
                let (message, response) = payload();
                transmitter
                    .handle_message(message)
                    .await
                    .expect("payload should be queued");
                responses.push(response);
            }

            assert_eq!(transmitter.pending.len(), PENDING_CAPACITY);
            let error = responses[0]
                .try_recv()
                .expect("evicted payload should be answered")
                .expect_err("evicted payload should be rejected");
            assert_eq!(error.kind(), ErrorKind::OutOfMemory);
            assert!(responses[1].try_recv().is_err());
        });
    }
}
