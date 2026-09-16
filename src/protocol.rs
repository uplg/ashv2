//! Protocol definitions for the communication layer.

pub use self::control_byte::ControlByte;
pub use self::randomization::Mask;
pub use self::stuffing::{Stuff, Unstuff};
use crate::SEQ_MASK;

mod control_byte;
mod randomization;
mod stuffing;

/// Number of sequence positions advanced after a frame.
const SEQUENCE_STEP: u8 = 1;

/// Returns the following sequence number, wrapping within the three-bit ASH field.
pub const fn next_sequence_number(sequence: u8) -> u8 {
    sequence.wrapping_add(SEQUENCE_STEP) & SEQ_MASK
}
