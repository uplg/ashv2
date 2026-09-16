use std::fmt::Display;
use std::io;

use tokio::sync::oneshot::Sender;

use crate::Payload;
use crate::frame::{Error, Rst, RstAck};
use crate::hex_slice::HexSlice;

/// Messages sent to the `ASHv2` transmitter.
#[derive(Debug)]
#[cfg_attr(target_pointer_width = "64", expect(variant_size_differences))]
pub enum Message {
    /// Payload received from the network.
    Payload {
        /// Data payload to send.
        payload: Box<Payload>,
        /// Response channel to notify when the payload has been sent.
        response_tx: Sender<io::Result<()>>,
    },

    /// Send an ACK frame with the given ack number.
    SendAck(u8),

    /// Send a NAK frame with the given ack number.
    SendNak(u8),

    /// Received RST frame.
    Rst(Rst),

    /// Received RST-ACK frame.
    RstAck(RstAck),

    /// Received ERROR frame.
    Error(Error),

    /// Received acknowledgement carrying the peer's ACK number.
    ReceivedAck(u8),

    /// Received negative acknowledgement carrying the peer's ACK number.
    ReceivedNak(u8),
}

impl Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Payload { payload, .. } => write!(f, "Payload({:#04X})", HexSlice::new(payload)),
            Self::SendAck(ack_num) => write!(f, "SendAck({ack_num})"),
            Self::SendNak(ack_num) => write!(f, "SendNak({ack_num})"),
            Self::Rst(rst) => write!(f, "Rst({rst})"),
            Self::RstAck(rst_ack) => write!(f, "RstAck({rst_ack})"),
            Self::Error(error) => write!(f, "Error({error})"),
            Self::ReceivedAck(ack_num) => write!(f, "ReceivedAck({ack_num})"),
            Self::ReceivedNak(ack_num) => write!(f, "ReceivedNak({ack_num})"),
        }
    }
}
