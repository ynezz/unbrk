//! U-Boot command execution and output parsing helpers.

use crate::error::{ConsoleTail, UnbrkError};
use crate::event::RecoveryStage;
use crate::prompt::find_prompt_allowing_trailing_space_with_regex;
use crate::target::PromptPattern;
use crate::transport::Transport;
use regex::{Regex, bytes::Regex as BytesRegex};
use std::time::Duration;

/// Default timeout for one U-Boot command round-trip.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Parsed `loadaddr` value from `printenv loadaddr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadAddr(u32);

impl LoadAddr {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Parsed `filesize` value from `printenv filesize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSize(u64);

impl FileSize {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Parsed `Total Size` summary emitted by `loadx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferSize {
    pub hex_bytes: u64,
    pub decimal_bytes: u64,
}

/// Successful `mmc erase` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmcEraseSuccess;

/// Successful `mmc write` confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmcWriteSuccess;

/// Captured command round-trip output up to the prompt reappearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UBootCommandOutput {
    bytes: Vec<u8>,
}

impl UBootCommandOutput {
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    #[must_use]
    pub fn as_lossy_str(&self) -> String {
        String::from_utf8_lossy(self.as_bytes()).into_owned()
    }
}

/// Executes one U-Boot command and waits for the prompt to reappear.
///
/// The captured output may begin with an echoed `AN7581> <command>` line, so
/// completion detection intentionally ignores a leading prompt match on the
/// first echoed line and waits for the prompt to appear again later.
///
/// # Errors
///
/// Returns serial, timeout, or protocol errors while executing the command.
pub fn run_command(
    transport: &mut impl Transport,
    prompt: PromptPattern,
    command: &str,
    timeout: Duration,
) -> Result<UBootCommandOutput, UnbrkError> {
    let regex = prompt
        .compile()
        .map_err(|error| compile_prompt_error(&error))?;
    transport
        .set_timeout(timeout)
        .map_err(|source| UnbrkError::Serial {
            operation: "setting the U-Boot command timeout",
            source,
        })?;

    let mut command_line = command.as_bytes().to_vec();
    command_line.push(b'\n');

    transport
        .write(&command_line)
        .map_err(|source| UnbrkError::Serial {
            operation: "writing the U-Boot command",
            source,
        })?;
    transport.flush().map_err(|source| UnbrkError::Serial {
        operation: "flushing the U-Boot command",
        source,
    })?;

    let mut output = Vec::new();
    let mut scratch = [0_u8; 256];

    loop {
        match transport.read(&mut scratch) {
            Ok(0) => {
                return Err(UnbrkError::Timeout {
                    stage: RecoveryStage::UBoot,
                    operation: "the U-Boot prompt after a command",
                    timeout,
                    recent_console: ConsoleTail::new(output),
                });
            }
            Ok(read_len) => output.extend_from_slice(&scratch[..read_len]),
            Err(source) if source.kind() == std::io::ErrorKind::TimedOut => {
                return Err(UnbrkError::Timeout {
                    stage: RecoveryStage::UBoot,
                    operation: "the U-Boot prompt after a command",
                    timeout,
                    recent_console: ConsoleTail::new(output),
                });
            }
            Err(source) => {
                return Err(UnbrkError::Serial {
                    operation: "reading U-Boot command output",
                    source,
                });
            }
        }

        let search_cursor = prompt_search_cursor(&regex, &output);
        if find_prompt_allowing_trailing_space_with_regex(&regex, &output, search_cursor).is_some()
        {
            return Ok(UBootCommandOutput::new(output));
        }
    }
}

/// Parses `printenv loadaddr` output.
///
/// # Errors
///
/// Returns a protocol error when the output does not contain a parseable
/// `loadaddr=` assignment.
pub fn parse_loadaddr(output: &UBootCommandOutput) -> Result<LoadAddr, UnbrkError> {
    let value = capture_hex_value(output, r"loadaddr=([0-9a-fA-Fx]+)", "U-Boot loadaddr")?;
    let parsed = u32::try_from(parse_u_boot_hex(value.as_str(), output)?).map_err(|_| {
        UnbrkError::Protocol {
            stage: RecoveryStage::UBoot,
            detail: format!("U-Boot loadaddr {value} does not fit in u32"),
            recent_console: ConsoleTail::from_buffer(output.as_bytes()),
        }
    })?;

    Ok(LoadAddr::new(parsed))
}

/// Parses `printenv filesize` output.
///
/// # Errors
///
/// Returns a protocol error when the output does not contain a parseable
/// `filesize=` assignment.
pub fn parse_filesize(output: &UBootCommandOutput) -> Result<FileSize, UnbrkError> {
    let value = capture_hex_value(output, r"filesize=([0-9a-fA-Fx]+)", "U-Boot filesize")?;
    Ok(FileSize::new(parse_u_boot_hex(value.as_str(), output)?))
}

/// Verifies that `mmc erase` reported success.
///
/// # Errors
///
/// Returns a protocol error when the command output does not contain the
/// expected erase-success marker.
pub fn parse_mmc_erase_success(output: &UBootCommandOutput) -> Result<MmcEraseSuccess, UnbrkError> {
    require_output(output, r"(?i)blocks erased:\s+OK", "MMC erase")?;
    Ok(MmcEraseSuccess)
}

/// Verifies that `mmc write` reported success.
///
/// # Errors
///
/// Returns a protocol error when the command output does not contain the
/// expected write-success marker.
pub fn parse_mmc_write_success(output: &UBootCommandOutput) -> Result<MmcWriteSuccess, UnbrkError> {
    require_output(output, r"(?i)blocks written:\s+OK", "MMC write")?;
    Ok(MmcWriteSuccess)
}

/// Parses the `Total Size = 0x... = ... Bytes` summary from `loadx`.
///
/// # Panics
///
/// Panics only if the hard-coded `Total Size` regex is invalid.
///
/// # Errors
///
/// Returns a protocol error when the output does not contain a parseable
/// `Total Size` summary.
pub fn parse_total_size(output: &UBootCommandOutput) -> Result<TransferSize, UnbrkError> {
    let regex = Regex::new(r"Total Size\s*=\s*0x([0-9a-fA-F]+)\s*=\s*([0-9]+)\s*Bytes")
        .expect("static Total Size regex is valid");
    let text = output.as_lossy_str();
    let captures = regex
        .captures(text.as_str())
        .ok_or_else(|| missing_output_error(output, "loadx total size"))?;

    let hex_bytes = u64::from_str_radix(captures.get(1).expect("capture exists").as_str(), 16)
        .map_err(|error| malformed_output_error(output, "loadx total size hex", &error))?;
    let decimal_bytes = captures
        .get(2)
        .expect("capture exists")
        .as_str()
        .parse::<u64>()
        .map_err(|error| malformed_output_error(output, "loadx total size decimal", &error))?;

    Ok(TransferSize {
        hex_bytes,
        decimal_bytes,
    })
}

fn prompt_search_cursor(regex: &BytesRegex, output: &[u8]) -> usize {
    let Some(line_end) = first_line_end(output) else {
        return 0;
    };

    let Some(first_prompt) = find_prompt_allowing_trailing_space_with_regex(regex, output, 0)
    else {
        return 0;
    };

    if first_prompt.next_cursor <= line_end {
        line_end
    } else {
        0
    }
}

fn first_line_end(bytes: &[u8]) -> Option<usize> {
    let line_end = bytes
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))?;
    let mut cursor = line_end;

    while matches!(bytes.get(cursor), Some(byte) if byte.is_ascii_control()) {
        cursor += 1;
    }

    Some(cursor)
}

fn capture_hex_value(
    output: &UBootCommandOutput,
    pattern: &str,
    label: &str,
) -> Result<String, UnbrkError> {
    let regex = Regex::new(pattern).expect("static parser regex is valid");
    let text = output.as_lossy_str();
    let captures = regex
        .captures(text.as_str())
        .ok_or_else(|| missing_output_error(output, label))?;

    Ok(captures.get(1).expect("capture exists").as_str().to_owned())
}

fn parse_u_boot_hex(value: &str, output: &UBootCommandOutput) -> Result<u64, UnbrkError> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    u64::from_str_radix(digits, 16)
        .map_err(|error| malformed_output_error(output, "U-Boot hex value", &error))
}

fn require_output(
    output: &UBootCommandOutput,
    pattern: &str,
    label: &str,
) -> Result<(), UnbrkError> {
    let regex = Regex::new(pattern).expect("static parser regex is valid");
    if regex.is_match(output.as_lossy_str().as_str()) {
        Ok(())
    } else {
        Err(missing_output_error(output, label))
    }
}

fn missing_output_error(output: &UBootCommandOutput, label: &str) -> UnbrkError {
    UnbrkError::Protocol {
        stage: RecoveryStage::UBoot,
        detail: format!("{label} did not report success"),
        recent_console: ConsoleTail::from_buffer(output.as_bytes()),
    }
}

fn malformed_output_error(
    output: &UBootCommandOutput,
    label: &str,
    error: &impl std::fmt::Display,
) -> UnbrkError {
    UnbrkError::Protocol {
        stage: RecoveryStage::UBoot,
        detail: format!("failed to parse {label}: {error}"),
        recent_console: ConsoleTail::from_buffer(output.as_bytes()),
    }
}

fn compile_prompt_error(error: &regex::Error) -> UnbrkError {
    UnbrkError::Protocol {
        stage: RecoveryStage::UBoot,
        detail: format!("invalid U-Boot prompt regex: {error}"),
        recent_console: ConsoleTail::empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_COMMAND_TIMEOUT, FileSize, LoadAddr, TransferSize, UBootCommandOutput,
        parse_filesize, parse_loadaddr, parse_mmc_erase_success, parse_mmc_write_success,
        parse_total_size, run_command,
    };
    use crate::target::AN7581;
    use crate::transport::{MockStep, MockTransport};

    #[test]
    fn run_command_waits_for_the_prompt_to_reappear_after_an_echoed_prompt_line() {
        let mut transport = MockTransport::new([
            MockStep::SetTimeout(DEFAULT_COMMAND_TIMEOUT),
            MockStep::Write(b"printenv loadaddr\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> printenv loadaddr\r\nloadaddr=0x81800000\r\nAN7581> ".to_vec(),
            ),
        ]);

        let output = run_command(
            &mut transport,
            AN7581.prompts.uboot,
            "printenv loadaddr",
            DEFAULT_COMMAND_TIMEOUT,
        )
        .unwrap();

        assert!(output.as_lossy_str().contains("loadaddr=0x81800000"));
        transport.assert_finished();
    }

    #[test]
    fn parse_loadaddr_accepts_prefixed_hex() {
        let output = UBootCommandOutput::new(b"loadaddr=0x81800000\r\n".to_vec());

        assert_eq!(parse_loadaddr(&output).unwrap(), LoadAddr::new(0x8180_0000));
    }

    #[test]
    fn parse_filesize_accepts_hex_without_a_prefix() {
        let output = UBootCommandOutput::new(b"filesize=1f400\r\n".to_vec());

        assert_eq!(parse_filesize(&output).unwrap(), FileSize::new(0x1f400));
    }

    #[test]
    fn parse_filesize_accepts_hex_with_a_prefix() {
        let output = UBootCommandOutput::new(b"filesize=0x1f400\r\n".to_vec());

        assert_eq!(parse_filesize(&output).unwrap(), FileSize::new(0x1f400));
    }

    #[test]
    fn parse_mmc_erase_success_detects_the_expected_marker() {
        let output = UBootCommandOutput::new(b"2048 blocks erased: OK\r\n".to_vec());

        assert_eq!(
            parse_mmc_erase_success(&output).unwrap(),
            super::MmcEraseSuccess
        );
    }

    #[test]
    fn parse_mmc_write_success_detects_the_expected_marker() {
        let output = UBootCommandOutput::new(b"252 blocks written: OK\r\n".to_vec());

        assert_eq!(
            parse_mmc_write_success(&output).unwrap(),
            super::MmcWriteSuccess
        );
    }

    #[test]
    fn parse_total_size_returns_both_hex_and_decimal_counts() {
        let output = UBootCommandOutput::new(b"Total Size = 0x1F400 = 128000 Bytes\r\n".to_vec());

        assert_eq!(
            parse_total_size(&output).unwrap(),
            TransferSize {
                hex_bytes: 0x1f400,
                decimal_bytes: 128_000,
            }
        );
    }

    #[test]
    fn malformed_outputs_return_protocol_errors() {
        let output = UBootCommandOutput::new(b"filesize=0x\r\n".to_vec());
        let error = parse_filesize(&output).unwrap_err();

        assert!(error.to_string().contains("failed to parse"));

        let output = UBootCommandOutput::new(b"no success marker here\r\n".to_vec());
        let error = parse_mmc_write_success(&output).unwrap_err();
        assert!(error.to_string().contains("did not report success"));
    }
}
