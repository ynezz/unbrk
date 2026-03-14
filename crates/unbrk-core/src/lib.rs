//! Core recovery primitives for `unbrk`.

pub mod error;
pub mod event;
pub mod prompt;
pub mod target;
pub mod transport;
pub mod xmodem;

pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventKind, EventPayload, FailureClass, ImageKind, RecoveryStage,
    TransferStage,
};
pub use prompt::{PromptMatch, advance_to_prompt, find_prompt};
pub use transport::{DEFAULT_BAUD_RATE, MockStep, MockTransport, SerialTransport, Transport};
pub use xmodem::{
    CrcReadyMatch, XMODEM_CRC_READY_BYTE, XMODEM_CRC_READY_MIN_BYTES, advance_to_crc_ready,
    find_crc_ready,
};
