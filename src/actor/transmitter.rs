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
use tokio::time::sleep;

use self::buffer::Buffer;
use self::transmission::Transmission;
use crate::actor::message::Message;
use crate::frame::{Ack, Data, Error, Nak, RST, Rst, RstAck};
use crate::status::Status;
use crate::types::{MAX_FRAME_SIZE, Payload};
use crate::{SEQ_MASK, T_RSTACK_MAX_MILLIS, T_RX_ACK_MAX_MILLIS, TX_K};

mod buffer;
mod transmission;

/// Maximum time to wait for RST ACK frame after sending RST frame.
const T_RSTACK_MAX: Duration = Duration::from_millis(T_RSTACK_MAX_MILLIS);

const T_RX_ACK_MAX: Duration = Duration::from_millis(T_RX_ACK_MAX_MILLIS);

/// Delay between housekeeping ticks while there is backlog (pending messages
/// to retry or in-flight transmissions to watch for retransmission).
const TICK_DELAY: Duration = Duration::from_millis(100);

/// Maximum number of messages kept in the local pending queue.
///
/// The pending queue holds payloads that could not be transmitted yet (link
/// down or transmission window full). It is local to the transmitter: the
/// transmitter must NEVER send into its own bounded input channel, as that
/// deadlocks the whole actor once the channel fills up (the receiver also
/// produces into that channel and would block right behind it).
const PENDING_CAPACITY: usize = 64;

/// `ASHv2` transmitter.
#[derive(Debug)]
pub struct Transmitter<T> {
    buffer: Buffer<T>,
    messages: Receiver<Message>,
    /// Local queue of messages to retry (link down or window full).
    pending: VecDeque<Message>,
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
            let has_backlog = !self.pending.is_empty() || !self.transmissions.is_empty();

            let message = tokio::select! {
                maybe_message = self.messages.recv() => {
                    let Some(message) = maybe_message else {
                        break;
                    };
                    Some(message)
                }
                () = sleep(TICK_DELAY), if has_backlog => None,
            };

            let result = match message {
                Some(message) => {
                    trace!("Received message: {message}");
                    self.handle_message(message).await
                }
                None => self.tick().await,
            };

            if let Err(error) = result {
                error!("Resetting connection due to I/O error: {error}");
                self.status = Status::Failed;
            }
        }

        running.store(false, Relaxed);
        info!("Transmitter loop terminated.");
    }

    /// Periodic housekeeping: retransmit timed-out `DATA` frames and retry
    /// one pending message.
    async fn tick(&mut self) -> io::Result<()> {
        self.retransmit_timed_out().await?;

        if let Some(message) = self.pending.pop_front() {
            trace!("Retrying pending message: {message}");
            self.handle_message(message).await?;
        }

        Ok(())
    }

    async fn handle_message(&mut self, message: Message) -> io::Result<()> {
        if self.status != Status::Connected {
            if let Message::RstAck(ack) = message {
                return self.handle_rst_ack(ack).await;
            }

            // Only log if the connection has failed, not if it hasn't been established yet.
            if self.status == Status::Failed {
                warn!("ASHv2 Connection failed. Resetting...");
            }

            self.reset().await?;

            // Keep payloads for delivery after reconnection, but drop
            // link-control messages: they refer to the pre-reset frame space
            // and replaying them after a reconnect only corrupts the link.
            match message {
                payload @ Message::Payload { .. } => self.enqueue_pending(payload),
                other => trace!("Dropping link-control message while disconnected: {other}"),
            }

            return Ok(());
        }

        match message {
            Message::Payload {
                payload,
                response_tx: response,
            } => self.handle_payload(payload, response).await,
            Message::Ack(ack_num) => self.send_ack(ack_num).await,
            Message::Nak(ack_num) => self.send_nak(ack_num).await,
            Message::Rst(rst) => self.handle_rst(rst).await,
            Message::RstAck(rst_ack) => self.handle_rst_ack(rst_ack).await,
            Message::Error(error) => self.handle_error(error).await,
            Message::AckSentFrame(frame_num) => {
                self.ack_sent_frames(frame_num);
                Ok(())
            }
            Message::NakSentFrame(frame_num) => self.nak_sent_frames(frame_num).await,
        }
    }

    async fn handle_payload(
        &mut self,
        payload: Box<Payload>,
        response: tokio::sync::oneshot::Sender<io::Result<()>>,
    ) -> io::Result<()> {
        if self.transmissions.is_full() {
            warn!("Insufficient space in transmission queue for payload, requeueing.");
            self.enqueue_pending(Message::Payload {
                payload,
                response_tx: response,
            });
            return Ok(());
        }

        let data = Data::new(self.next_frame_number(), self.ack_number, *payload);
        // With a sliding windows size > 1 the NCP may enter an "ERROR: Assert" state when sending
        // fragmented messages if each DATA frame's ACK number is not increased.
        self.ack_number = self.ack_number.wrapping_add(1).bitand(SEQ_MASK);
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
                // Per the ASH specification, both sides restart frame numbering
                // from zero after a reset. In-flight transmissions are lost;
                // their EZSP commands will time out and be retried upstream.
                // Without this, the post-reset session starts with stale frame
                // numbers and wedges in an out-of-sequence NAK storm.
                self.frame_number = 0;
                self.ack_number = 0;
                self.transmissions.clear();
                Ok(())
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
        while let Some(transmission) = self
            .transmissions
            .iter()
            .position(|transmission| {
                transmission.frame_num().wrapping_add(1).bitand(SEQ_MASK)
                    == ack_num.bitand(SEQ_MASK)
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
        if let Some(transmission) = self
            .transmissions
            .iter()
            .position(|transmission| transmission.frame_num() == nak_num.bitand(SEQ_MASK))
            .map(|index| self.transmissions.remove(index))
        {
            debug!("Retransmitting NAK'ed frame #{}", transmission.frame_num());
            self.transmit(transmission).await?;
        }

        Ok(())
    }

    /// Retransmit `DATA` frames whose acknowledgement timed out.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if a frame exceeded its retransmission limit
    /// or the retransmission itself failed, meaning the link should be reset.
    async fn retransmit_timed_out(&mut self) -> io::Result<()> {
        while let Some(transmission) = self
            .transmissions
            .iter()
            .position(|transmission| transmission.is_timed_out(T_RX_ACK_MAX))
            .map(|index| self.transmissions.remove(index))
        {
            debug!(
                "Retransmitting timed-out frame #{} after {:?}",
                transmission.frame_num(),
                transmission.elapsed()
            );
            self.transmit(transmission).await?;
        }

        Ok(())
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
    fn next_frame_number(&mut self) -> u8 {
        let frame_number = self.frame_number;
        self.frame_number = self.frame_number.wrapping_add(1).bitand(SEQ_MASK);
        frame_number
    }

    /// Queue a message locally for a later retry, evicting the oldest entry
    /// when full. An evicted payload wakes its caller with an error because
    /// its response channel is dropped.
    fn enqueue_pending(&mut self, message: Message) {
        if self.pending.len() >= PENDING_CAPACITY {
            if let Some(dropped) = self.pending.pop_front() {
                warn!("Pending queue full, dropping oldest message: {dropped}");
            }
        }

        self.pending.push_back(message);
    }
}
