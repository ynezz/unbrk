//! Core recovery primitives for `unbrk`.

pub mod error;
pub mod event;
pub mod target;
pub mod transport;

pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventKind, EventPayload, FailureClass, ImageKind, RecoveryStage,
    TransferStage,
};
pub use transport::{DEFAULT_BAUD_RATE, MockStep, MockTransport, SerialTransport, Transport};
