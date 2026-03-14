//! Core recovery primitives for `unbrk`.

pub mod error;
pub mod event;
pub mod prompt;
pub mod recovery;
pub mod target;
pub mod transport;
pub mod uboot;
pub mod xmodem;

pub use event::{
    EVENT_SCHEMA_VERSION, Event, EventKind, EventPayload, FailureClass, ImageKind, RecoveryStage,
    TransferStage,
};
pub use prompt::{PromptMatch, advance_to_prompt, find_prompt};
pub use recovery::{
    DEFAULT_PROMPT_TIMEOUT, RecoveryConfig, RecoveryImages, RecoveryReport, RecoveryState,
    recover_to_uboot,
};
pub use transport::{DEFAULT_BAUD_RATE, MockStep, MockTransport, SerialTransport, Transport};
pub use uboot::{
    DEFAULT_COMMAND_TIMEOUT, FileSize, LoadAddr, MmcEraseSuccess, MmcWriteSuccess, TransferSize,
    UBootCommandOutput, parse_filesize, parse_loadaddr, parse_mmc_erase_success,
    parse_mmc_write_success, parse_total_size, run_command,
};
pub use xmodem::{
    CrcReadyMatch, XMODEM_CRC_READY_BYTE, XMODEM_CRC_READY_MIN_BYTES, advance_to_crc_ready,
    find_crc_ready,
};
