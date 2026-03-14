//! Core recovery primitives for `unbrk`.

pub mod error;
pub mod event;
pub mod flash;
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
pub use flash::{DEFAULT_RESET_TIMEOUT, FlashConfig, FlashReport, flash_from_uboot};
pub use prompt::{PromptMatch, advance_to_prompt, find_prompt};
pub use recovery::{
    DEFAULT_PROMPT_TIMEOUT, RecoveryConfig, RecoveryImages, RecoveryReport, RecoveryState,
    recover_to_uboot,
};
pub use transport::{DEFAULT_BAUD_RATE, MockStep, MockTransport, SerialTransport, Transport};
pub use uboot::{
    DEFAULT_COMMAND_TIMEOUT, FileSize, LoadAddr, MmcEraseSuccess, MmcWriteSuccess, TransferSize,
    UBootCommandOutput, parse_filesize, parse_loadaddr, parse_mmc_erase_success,
    parse_mmc_write_success, parse_optional_total_size, parse_total_size, run_command,
};
pub use xmodem::{
    CrcReadyMatch, XMODEM_CRC_READY_BYTE, XMODEM_CRC_READY_MIN_BYTES, advance_to_crc_ready,
    find_crc_ready,
};

#[cfg(test)]
mod tests {
    use super::{UBootCommandOutput, parse_optional_total_size};

    #[test]
    fn crate_root_reexports_parse_optional_total_size() {
        let output = UBootCommandOutput::new(b"Total Size = 0x4 = 4 Bytes\r\n".to_vec());

        let parsed = parse_optional_total_size(&output).unwrap();

        assert_eq!(
            parsed.map(|size| (size.hex_bytes, size.decimal_bytes)),
            Some((4, 4))
        );
    }
}
