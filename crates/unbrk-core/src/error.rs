//! Core error taxonomy for recovery and flash operations.

use crate::event::{FailureClass, ImageKind, RecoveryStage, TransferStage};
use std::borrow::Cow;
use std::fmt;
use std::io;
use std::time::Duration;
use thiserror::Error;

/// Maximum number of trailing console bytes kept for diagnostics.
pub const MAX_CONSOLE_TAIL_BYTES: usize = 200;

/// Stable exit codes shared with the CLI contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StableExitCode {
    Success = 0,
    SerialError = 1,
    Timeout = 2,
    ProtocolError = 3,
    XmodemFailure = 4,
    UBootCommandFailure = 5,
    VerificationMismatch = 6,
    BadInput = 7,
    UserAbort = 8,
}

impl StableExitCode {
    /// Returns the numeric exit status exposed to shells and automation.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Truncated tail of recent console bytes for recovery diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleTail {
    bytes: Vec<u8>,
}

impl ConsoleTail {
    /// Creates a console snapshot from the trailing bytes of a UART buffer.
    #[must_use]
    pub fn new(bytes: impl AsRef<[u8]>) -> Self {
        Self::from_buffer(bytes.as_ref())
    }

    /// Creates a console snapshot from the trailing bytes of a borrowed UART buffer.
    #[must_use]
    pub fn from_buffer(bytes: &[u8]) -> Self {
        let start = bytes.len().saturating_sub(MAX_CONSOLE_TAIL_BYTES);

        Self {
            bytes: bytes[start..].to_vec(),
        }
    }

    /// Creates an empty console snapshot.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Returns the retained raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Returns the retained bytes decoded with replacement characters.
    #[must_use]
    pub fn as_lossy_str(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.as_bytes())
    }

    /// Returns whether this snapshot contains no console output.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Default for ConsoleTail {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Display for ConsoleTail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            formatter.write_str("<no console output>")
        } else {
            formatter.write_str(self.as_lossy_str().as_ref())
        }
    }
}

/// Typed recovery/flash error with stable failure-class and exit-code mapping.
#[derive(Debug, Error)]
pub enum UnbrkError {
    #[error("serial I/O failed while {operation}: {source}")]
    Serial {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(
        "timed out after {timeout:?} while waiting for {operation} during {stage}. Last console output: {recent_console}"
    )]
    Timeout {
        stage: RecoveryStage,
        operation: &'static str,
        timeout: Duration,
        recent_console: ConsoleTail,
    },
    #[error(
        "prompt mismatch during {stage}: expected {expected_pattern:?}, observed {observed:?}. Last console output: {recent_console}"
    )]
    PromptMismatch {
        stage: RecoveryStage,
        expected_pattern: String,
        observed: String,
        recent_console: ConsoleTail,
    },
    #[error("{stage} protocol error: {detail}. Last console output: {recent_console}")]
    Protocol {
        stage: RecoveryStage,
        detail: String,
        recent_console: ConsoleTail,
    },
    #[error("{stage} XMODEM failure: {detail}. Last console output: {recent_console}")]
    Xmodem {
        stage: TransferStage,
        detail: String,
        recent_console: ConsoleTail,
    },
    #[error("U-Boot command `{command}` failed: {detail}. Last console output: {recent_console}")]
    UBootCommand {
        command: String,
        detail: String,
        recent_console: ConsoleTail,
    },
    #[error(
        "verification mismatch for {image}: expected {expected_bytes} bytes, observed {observed_bytes}. Last console output: {recent_console}"
    )]
    VerificationMismatch {
        image: ImageKind,
        expected_bytes: u64,
        observed_bytes: u64,
        recent_console: ConsoleTail,
    },
    #[error("bad input: {message}")]
    BadInput { message: String },
    #[error("user abort: {message}")]
    UserAbort { message: String },
}

impl UnbrkError {
    /// Returns the stable failure class used by the event stream and CLI.
    #[must_use]
    pub const fn failure_class(&self) -> FailureClass {
        match self {
            Self::Serial { .. } => FailureClass::Serial,
            Self::Timeout { .. } => FailureClass::Timeout,
            Self::PromptMismatch { .. } | Self::Protocol { .. } => FailureClass::Protocol,
            Self::Xmodem { .. } => FailureClass::Xmodem,
            Self::UBootCommand { .. } => FailureClass::UBootCommand,
            Self::VerificationMismatch { .. } => FailureClass::VerificationMismatch,
            Self::BadInput { .. } => FailureClass::BadInput,
            Self::UserAbort { .. } => FailureClass::UserAbort,
        }
    }

    /// Returns the documented process exit code for this failure.
    #[must_use]
    pub const fn exit_code(&self) -> StableExitCode {
        match self.failure_class() {
            FailureClass::Serial => StableExitCode::SerialError,
            FailureClass::Timeout => StableExitCode::Timeout,
            FailureClass::Protocol => StableExitCode::ProtocolError,
            FailureClass::Xmodem => StableExitCode::XmodemFailure,
            FailureClass::UBootCommand => StableExitCode::UBootCommandFailure,
            FailureClass::VerificationMismatch => StableExitCode::VerificationMismatch,
            FailureClass::BadInput => StableExitCode::BadInput,
            FailureClass::UserAbort => StableExitCode::UserAbort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsoleTail, MAX_CONSOLE_TAIL_BYTES, StableExitCode, UnbrkError};
    use crate::event::{FailureClass, ImageKind, RecoveryStage, TransferStage};
    use std::io;
    use std::time::Duration;

    #[test]
    fn console_tail_keeps_the_last_bytes_only() {
        let input = vec![b'x'; MAX_CONSOLE_TAIL_BYTES + 25];
        let tail = ConsoleTail::new(input);

        assert_eq!(tail.as_bytes().len(), MAX_CONSOLE_TAIL_BYTES);
    }

    #[test]
    fn console_tail_from_buffer_matches_owned_constructor() {
        let input = vec![b'y'; MAX_CONSOLE_TAIL_BYTES + 25];

        assert_eq!(
            ConsoleTail::from_buffer(input.as_slice()),
            ConsoleTail::new(input)
        );
    }

    #[test]
    fn empty_console_tail_has_a_placeholder_display() {
        let tail = ConsoleTail::empty();

        assert_eq!(tail.to_string(), "<no console output>");
    }

    #[test]
    fn timeout_errors_map_to_timeout_failure_contract() {
        let error = UnbrkError::Timeout {
            stage: RecoveryStage::PreloaderPrompt,
            operation: "initial prompt",
            timeout: Duration::from_secs(3),
            recent_console: ConsoleTail::new(b"Press "),
        };

        assert_eq!(error.failure_class(), FailureClass::Timeout);
        assert_eq!(error.exit_code(), StableExitCode::Timeout);
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn prompt_mismatches_map_to_protocol_failures() {
        let error = UnbrkError::PromptMismatch {
            stage: RecoveryStage::FipPrompt,
            expected_pattern: String::from("Press x to load BL31 \\+ U-Boot FIP"),
            observed: String::from("Booting"),
            recent_console: ConsoleTail::new(b"Booting\r\n"),
        };

        assert_eq!(error.failure_class(), FailureClass::Protocol);
        assert_eq!(error.exit_code().code(), 3);
        assert!(error.to_string().contains("prompt mismatch"));
    }

    #[test]
    fn xmodem_errors_map_to_transfer_failure_contract() {
        let error = UnbrkError::Xmodem {
            stage: TransferStage::Fip,
            detail: String::from("receiver cancelled"),
            recent_console: ConsoleTail::new(vec![0x15, 0x15]),
        };

        assert_eq!(error.failure_class(), FailureClass::Xmodem);
        assert_eq!(error.exit_code(), StableExitCode::XmodemFailure);
    }

    #[test]
    fn verification_mismatches_map_to_the_expected_exit_code() {
        let error = UnbrkError::VerificationMismatch {
            image: ImageKind::Preloader,
            expected_bytes: 129_024,
            observed_bytes: 128_000,
            recent_console: ConsoleTail::new(b"filesize=1f400"),
        };

        assert_eq!(error.failure_class(), FailureClass::VerificationMismatch);
        assert_eq!(error.exit_code(), StableExitCode::VerificationMismatch);
    }

    #[test]
    fn serial_errors_preserve_the_underlying_source() {
        let error = UnbrkError::Serial {
            operation: "open serial port",
            source: io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"),
        };

        assert_eq!(error.failure_class(), FailureClass::Serial);
        assert_eq!(error.exit_code(), StableExitCode::SerialError);
        assert!(error.to_string().contains("permission denied"));
    }
}
