use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;

use either::{Either, Left, Right};
use serialport::SerialPort;
use tokio::spawn;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::SendError;
use tokio::task::{JoinError, JoinHandle};

use crate::actor::message::Message;
use crate::actor::{Receiver, Transmitter};

/// Sender and receiver tasks wrapper to allow termination.
#[derive(Debug)]
pub struct Tasks<T> {
    transmitter: JoinHandle<T>,
    receiver: JoinHandle<()>,
    sender: Sender<Message>,
    running: Arc<AtomicBool>,
}

impl<T> Tasks<T>
where
    T: SerialPort + Sync + 'static,
{
    /// Crate new tasks.
    pub(crate) fn spawn(
        transmitter: Transmitter<T>,
        receiver: Receiver<T>,
        sender: Sender<Message>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        Self {
            transmitter: spawn(transmitter.run()),
            receiver: spawn(receiver.run(running.clone())),
            sender,
            running,
        }
    }
}

impl<T> Tasks<T> {
    /// Returns `true` if both the transmitter and receiver tasks are still running.
    ///
    /// When either task has finished (due to a panic, error, or normal exit),
    /// this returns `false`, indicating that the ASH transport is no longer healthy
    /// and the pipeline should be rebuilt.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.transmitter.is_finished() && !self.receiver.is_finished()
    }

    /// Terminate the tasks.
    ///
    /// # Errors
    ///
    /// Returns either
    /// - a [`SendError`] if sending the termination message fails, or
    /// - a [`JoinError`] if joining either task fails.
    pub async fn terminate(self) -> Result<T, Either<SendError<Message>, JoinError>> {
        self.running.store(false, Relaxed);
        self.sender.send(Message::Terminate).await.map_err(Left)?;
        let serial_port = self.transmitter.await.map_err(Right)?;
        self.receiver.await.map_err(Right)?;
        Ok(serial_port)
    }
}
