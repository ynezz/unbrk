//! Persistent flash-plan execution from a live U-Boot prompt.

use crate::error::{ConsoleTail, UnbrkError};
use crate::event::{Event, EventPayload, EventRecorder, ImageKind, RecoveryStage, TransferStage};
use crate::prompt::{PromptMatch, advance_to_prompt_allowing_trailing_space_with_regex};
use crate::target::{BlockCount, FlashPlan, TargetProfile, WriteStage};
use crate::transport::Transport;
use crate::uboot::{
    DEFAULT_COMMAND_TIMEOUT, FileSize, LoadAddr, UBootCommandOutput, parse_filesize,
    parse_loadaddr, parse_mmc_erase_success, parse_mmc_write_success, parse_optional_total_size,
    run_command,
};
use crate::xmodem::{
    CrcReadyMatch, XmodemConfig, XmodemTransferReport, advance_to_crc_ready, send_crc,
};
use regex::bytes::Regex;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// Default timeout for observing post-flash reset output.
pub const DEFAULT_RESET_TIMEOUT: Duration = Duration::from_secs(20);

const RESET_EVIDENCE_PATTERN: &str = r"EcoNet System Reset|Press x|U-Boot";

/// Tunables for the destructive flash plan executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashConfig {
    pub command_timeout: Duration,
    pub reset_timeout: Duration,
    pub xmodem: XmodemConfig,
}

impl FlashConfig {
    #[must_use]
    pub const fn new(
        command_timeout: Duration,
        reset_timeout: Duration,
        xmodem: XmodemConfig,
    ) -> Self {
        Self {
            command_timeout,
            reset_timeout,
            xmodem,
        }
    }
}

impl Default for FlashConfig {
    fn default() -> Self {
        Self::new(
            DEFAULT_COMMAND_TIMEOUT,
            DEFAULT_RESET_TIMEOUT,
            XmodemConfig::default(),
        )
    }
}

/// Final outcome of a successful persistent-flash sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashReport {
    pub events: Vec<Event>,
    pub console: Vec<u8>,
    pub loadaddr: LoadAddr,
    pub reset_evidence: String,
    pub preloader_recovered_from_eot_quirk: bool,
    pub fip_recovered_from_eot_quirk: bool,
}

/// Runs the documented destructive flash sequence from an active U-Boot prompt.
///
/// # Errors
///
/// Returns a typed error when image validation fails before erase, when a
/// U-Boot command does not complete successfully, when XMODEM does not make
/// forward progress to the prompt, or when reset evidence never appears.
pub fn flash_from_uboot<F>(
    transport: &mut impl Transport,
    target: TargetProfile,
    plan: &FlashPlan,
    config: FlashConfig,
    observer: F,
) -> Result<FlashReport, UnbrkError>
where
    F: FnMut(&Event),
{
    let prepared_stages = prepare_stage_payloads(plan)?;

    let mut runner = FlashRunner::new(transport, target, config, observer)?;

    runner.ensure_prompt()?;

    let loadaddr_output = runner.run_uboot_command("printenv loadaddr")?;
    let loadaddr = parse_loadaddr(&loadaddr_output)?;
    runner.emit_command_completed("printenv loadaddr", "loadaddr read");

    let erase_range = plan
        .erase_ranges
        .first()
        .ok_or_else(|| UnbrkError::BadInput {
            message: String::from("flash plan has no erase range"),
        })?;
    let erase_command = format!(
        "mmc erase {:#x} {:#x}",
        erase_range.start_block.get(),
        erase_range.block_count.get(),
    );
    let erase_output = runner.run_uboot_command(erase_command.as_str())?;
    parse_mmc_erase_success(&erase_output)?;
    runner.emit_command_completed(erase_command.as_str(), "erase completed");

    let mut preloader_recovered_from_eot_quirk = false;
    let mut fip_recovered_from_eot_quirk = false;

    for prepared_stage in &prepared_stages {
        let stage_result = runner.transfer_stage(&prepared_stage.stage, &prepared_stage.payload)?;
        match prepared_stage.stage.image {
            ImageKind::Preloader => {
                preloader_recovered_from_eot_quirk = stage_result.recovered_from_eot_quirk;
            }
            ImageKind::Fip => {
                fip_recovered_from_eot_quirk = stage_result.recovered_from_eot_quirk;
            }
        }
    }

    let reset_evidence = runner.reset_and_wait()?;
    runner.emit(EventPayload::ResetSeen {
        evidence: reset_evidence.clone(),
    });
    runner.emit(EventPayload::HandoffReady {
        interactive_console: false,
    });

    Ok(FlashReport {
        events: runner.event_recorder.into_events(),
        console: runner.console,
        loadaddr,
        reset_evidence,
        preloader_recovered_from_eot_quirk,
        fip_recovered_from_eot_quirk,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StageTransferOutcome {
    recovered_from_eot_quirk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedStage {
    stage: WriteStage,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TotalSizeVerification {
    Verified,
    Skipped,
}

struct FlashRunner<'a, T, O> {
    transport: &'a mut T,
    target: TargetProfile,
    config: FlashConfig,
    uboot_prompt_regex: Regex,
    reset_evidence_regex: Regex,
    console: Vec<u8>,
    cursor: usize,
    event_recorder: EventRecorder<O>,
}

impl<'a, T, O> FlashRunner<'a, T, O>
where
    T: Transport,
    O: FnMut(&Event),
{
    fn new(
        transport: &'a mut T,
        target: TargetProfile,
        config: FlashConfig,
        observer: O,
    ) -> Result<Self, UnbrkError> {
        let uboot_prompt_regex = target
            .prompts
            .uboot
            .compile()
            .map_err(|error| Self::invalid_prompt_regex(&error, RecoveryStage::UBoot))?;

        Ok(Self {
            transport,
            target,
            config,
            uboot_prompt_regex,
            reset_evidence_regex: Regex::new(RESET_EVIDENCE_PATTERN)
                .map_err(|error| Self::invalid_prompt_regex(&error, RecoveryStage::FlashPlan))?,
            console: Vec::new(),
            cursor: 0,
            event_recorder: EventRecorder::new(observer),
        })
    }

    fn ensure_prompt(&mut self) -> Result<(), UnbrkError> {
        self.transport
            .set_timeout(self.config.command_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "setting the U-Boot prompt timeout",
                source,
            })?;
        self.transport
            .write(b"\r")
            .map_err(|source| UnbrkError::Serial {
                operation: "writing carriage return to wake U-Boot",
                source,
            })?;
        self.transport
            .flush()
            .map_err(|source| UnbrkError::Serial {
                operation: "flushing carriage return to wake U-Boot",
                source,
            })?;
        let prompt = self.read_uboot_prompt("an active U-Boot prompt")?;
        self.emit(EventPayload::UBootPromptSeen {
            prompt: prompt.prompt,
        });
        Ok(())
    }

    fn run_uboot_command(&mut self, command: &str) -> Result<UBootCommandOutput, UnbrkError> {
        self.emit(EventPayload::UBootCommandStarted {
            command: command.to_owned(),
        });
        let output = run_command(
            self.transport,
            &self.target.prompts.uboot,
            command,
            self.config.command_timeout,
        )?;
        self.console.extend_from_slice(output.as_bytes());
        self.cursor = self.console.len();
        Ok(output)
    }

    fn emit_command_completed(&mut self, command: &str, summary: &str) {
        self.emit(EventPayload::UBootCommandCompleted {
            command: command.to_owned(),
            success: true,
            summary: Some(summary.to_owned()),
        });
    }

    fn transfer_stage(
        &mut self,
        stage: &WriteStage,
        payload: &[u8],
    ) -> Result<StageTransferOutcome, UnbrkError> {
        let transfer_stage = match stage.image {
            ImageKind::Preloader => TransferStage::LoadxPreloader,
            ImageKind::Fip => TransferStage::LoadxFip,
        };
        let loadx_command = format!("loadx $loadaddr {}", self.target.serial.baud_rate);
        let command_start = self.console.len();
        self.start_loadx(loadx_command.as_str())?;
        let recovered_from_eot_quirk =
            self.run_loadx_transfer(transfer_stage, stage.image_path.as_path(), payload)?;

        let output = UBootCommandOutput::new(self.console[command_start..].to_vec());
        let total_size = Self::verify_total_size(stage.image, payload, &output)?;
        self.emit_command_completed(
            loadx_command.as_str(),
            match total_size {
                TotalSizeVerification::Verified => "loadx completed",
                TotalSizeVerification::Skipped => "loadx completed; Total Size summary absent",
            },
        );

        let filesize_output = self.run_uboot_command("printenv filesize")?;
        let observed_size = parse_filesize(&filesize_output)?;
        self.verify_filesize(stage.image, payload, observed_size, &filesize_output)?;
        self.emit_command_completed("printenv filesize", "filesize verified");

        self.write_stage_to_mmc(stage, payload)?;

        Ok(StageTransferOutcome {
            recovered_from_eot_quirk,
        })
    }

    fn start_loadx(&mut self, loadx_command: &str) -> Result<(), UnbrkError> {
        self.emit(EventPayload::UBootCommandStarted {
            command: loadx_command.to_owned(),
        });
        self.transport
            .set_timeout(self.config.command_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "setting the loadx timeout",
                source,
            })?;
        let mut loadx_line = loadx_command.as_bytes().to_vec();
        loadx_line.push(b'\n');
        self.transport
            .write(&loadx_line)
            .map_err(|source| UnbrkError::Serial {
                operation: "writing the loadx command",
                source,
            })?;
        self.transport.flush().map_err(|source| UnbrkError::Serial {
            operation: "flushing the loadx command",
            source,
        })
    }

    fn run_loadx_transfer(
        &mut self,
        transfer_stage: TransferStage,
        image_path: &Path,
        payload: &[u8],
    ) -> Result<bool, UnbrkError> {
        let crc_ready = self.read_crc_ready("XMODEM CRC readiness during loadx")?;
        self.emit(EventPayload::CrcReady {
            stage: transfer_stage,
            readiness_bytes_seen: crc_ready.readiness_bytes_seen,
        });
        self.emit(EventPayload::XmodemStarted {
            stage: transfer_stage,
            file_name: file_name(image_path),
            size_bytes: u64::try_from(payload.len()).unwrap_or(u64::MAX),
        });

        let transfer = self.send_loadx_payload(transfer_stage, payload);
        match transfer {
            Ok(report) => {
                self.emit_xmodem_completed(report, false);
                let prompt = self.read_uboot_prompt("the U-Boot prompt after loadx")?;
                self.emit(EventPayload::UBootPromptSeen {
                    prompt: prompt.prompt,
                });
                Ok(false)
            }
            Err(error) => self
                .recover_or_fail_from_loadx_error(&error, transfer_stage, payload)
                .map(|()| true),
        }
    }

    fn send_loadx_payload(
        &mut self,
        transfer_stage: TransferStage,
        payload: &[u8],
    ) -> Result<XmodemTransferReport, crate::xmodem::XmodemError> {
        let transport = &mut *self.transport;
        let event_recorder = &mut self.event_recorder;
        send_crc(
            transport,
            transfer_stage,
            payload,
            self.config.xmodem,
            |progress| {
                event_recorder.emit(EventPayload::XmodemProgress {
                    stage: progress.stage,
                    bytes_sent: progress.bytes_sent,
                    total_bytes: progress.total_bytes,
                });
            },
        )
    }

    fn recover_or_fail_from_loadx_error(
        &mut self,
        xmodem_error: &crate::xmodem::XmodemError,
        transfer_stage: TransferStage,
        payload: &[u8],
    ) -> Result<(), UnbrkError> {
        if !xmodem_error.permits_prompt_completion_recovery() {
            return Err(self.xmodem_error(xmodem_error, transfer_stage));
        }

        let prompt = self
            .read_uboot_prompt("the U-Boot prompt after loadx")
            .map_err(|prompt_error| {
                self.xmodem_failure(xmodem_error, &prompt_error, transfer_stage)
            })?;
        let size_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        self.emit(EventPayload::XmodemCompleted {
            stage: transfer_stage,
            bytes_sent: size_bytes,
            expected_bytes: size_bytes,
            recovered_from_eot_quirk: true,
        });
        self.emit(EventPayload::UBootPromptSeen {
            prompt: prompt.prompt,
        });
        Ok(())
    }

    fn verify_total_size(
        image: ImageKind,
        payload: &[u8],
        output: &UBootCommandOutput,
    ) -> Result<TotalSizeVerification, UnbrkError> {
        let Some(total_size) = parse_optional_total_size(output)? else {
            return Ok(TotalSizeVerification::Skipped);
        };
        let expected_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        if total_size.decimal_bytes == expected_bytes && total_size.hex_bytes == expected_bytes {
            return Ok(TotalSizeVerification::Verified);
        }
        let observed_bytes = if total_size.hex_bytes == expected_bytes {
            total_size.decimal_bytes
        } else {
            total_size.hex_bytes
        };

        Err(UnbrkError::VerificationMismatch {
            image,
            expected_bytes,
            observed_bytes,
            recent_console: ConsoleTail::from_buffer(output.as_bytes()),
        })
    }

    fn write_stage_to_mmc(&mut self, stage: &WriteStage, payload: &[u8]) -> Result<(), UnbrkError> {
        let block_count = payload_block_count(self.target.flash.block_size, payload);
        let write_command = format!(
            "mmc write $loadaddr {:#x} {:#x}",
            stage.start_block.get(),
            block_count.get(),
        );
        let write_output = self.run_uboot_command(write_command.as_str())?;
        parse_mmc_write_success(&write_output)?;
        self.emit_command_completed(write_command.as_str(), "write completed");
        Ok(())
    }

    fn verify_filesize(
        &mut self,
        image: ImageKind,
        payload: &[u8],
        observed_size: FileSize,
        output: &UBootCommandOutput,
    ) -> Result<(), UnbrkError> {
        let expected_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        let observed_bytes = observed_size.get();

        if observed_bytes != expected_bytes {
            return Err(UnbrkError::VerificationMismatch {
                image,
                expected_bytes,
                observed_bytes,
                recent_console: ConsoleTail::from_buffer(output.as_bytes()),
            });
        }

        self.emit(EventPayload::ImageVerified {
            image,
            expected_size_bytes: expected_bytes,
            observed_size_bytes: observed_bytes,
        });
        Ok(())
    }

    fn reset_and_wait(&mut self) -> Result<String, UnbrkError> {
        self.emit(EventPayload::UBootCommandStarted {
            command: String::from("reset"),
        });
        self.transport
            .set_timeout(self.config.reset_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "setting the reset-observation timeout",
                source,
            })?;
        self.transport
            .write(b"reset\n")
            .map_err(|source| UnbrkError::Serial {
                operation: "writing the reset command",
                source,
            })?;
        self.transport
            .flush()
            .map_err(|source| UnbrkError::Serial {
                operation: "flushing the reset command",
                source,
            })?;
        self.read_reset_evidence("post-flash reset output")
    }

    fn read_uboot_prompt(&mut self, operation: &'static str) -> Result<PromptMatch, UnbrkError> {
        let regex = self.uboot_prompt_regex.clone();
        self.transport
            .set_timeout(self.config.command_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "configuring the U-Boot prompt timeout",
                source,
            })?;

        loop {
            if let Some(prompt) = advance_to_prompt_allowing_trailing_space_with_regex(
                &regex,
                &self.console,
                &mut self.cursor,
            ) {
                return Ok(prompt);
            }

            self.read_more(RecoveryStage::UBoot, operation, self.config.command_timeout)?;
        }
    }

    fn read_crc_ready(&mut self, operation: &'static str) -> Result<CrcReadyMatch, UnbrkError> {
        self.transport
            .set_timeout(self.config.command_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "configuring the loadx CRC timeout",
                source,
            })?;

        loop {
            if let Some(readiness) = advance_to_crc_ready(&self.console, &mut self.cursor) {
                return Ok(readiness);
            }

            self.read_more(
                RecoveryStage::FlashPlan,
                operation,
                self.config.command_timeout,
            )?;
        }
    }

    fn read_reset_evidence(&mut self, operation: &'static str) -> Result<String, UnbrkError> {
        let regex = self.reset_evidence_regex.clone();
        loop {
            if let Some(bytes) = self.console.get(self.cursor..)
                && let Some(found) = regex.find(bytes)
            {
                let absolute_start = self.cursor + found.start();
                let absolute_end = self.cursor + found.end();
                self.cursor = absolute_end;
                return Ok(
                    String::from_utf8_lossy(&self.console[absolute_start..absolute_end])
                        .into_owned(),
                );
            }

            self.read_more(
                RecoveryStage::FlashPlan,
                operation,
                self.config.reset_timeout,
            )?;
        }
    }

    fn read_more(
        &mut self,
        stage: RecoveryStage,
        operation: &'static str,
        timeout: Duration,
    ) -> Result<(), UnbrkError> {
        let mut scratch = [0_u8; 256];
        match self.transport.read(&mut scratch) {
            Ok(0) => Err(UnbrkError::Timeout {
                stage,
                operation,
                timeout,
                recent_console: self.console_tail(),
            }),
            Ok(read_len) => {
                self.console.extend_from_slice(&scratch[..read_len]);
                Ok(())
            }
            Err(source) if source.kind() == std::io::ErrorKind::TimedOut => {
                Err(UnbrkError::Timeout {
                    stage,
                    operation,
                    timeout,
                    recent_console: self.console_tail(),
                })
            }
            Err(source) => Err(UnbrkError::Serial {
                operation: "reading flash-sequence console output",
                source,
            }),
        }
    }

    fn emit(&mut self, payload: EventPayload) {
        self.event_recorder.emit(payload);
    }

    fn emit_xmodem_completed(
        &mut self,
        report: XmodemTransferReport,
        recovered_from_eot_quirk: bool,
    ) {
        self.emit(EventPayload::XmodemCompleted {
            stage: report.stage,
            bytes_sent: report.bytes_sent,
            expected_bytes: report.total_bytes,
            recovered_from_eot_quirk,
        });
    }

    fn invalid_prompt_regex(error: &regex::Error, stage: RecoveryStage) -> UnbrkError {
        UnbrkError::Protocol {
            stage,
            detail: format!("invalid prompt regex: {error}"),
            recent_console: ConsoleTail::empty(),
        }
    }

    fn xmodem_failure(
        &self,
        xmodem_error: &crate::xmodem::XmodemError,
        prompt_error: &UnbrkError,
        stage: TransferStage,
    ) -> UnbrkError {
        UnbrkError::Xmodem {
            stage,
            detail: format!(
                "{xmodem_error}; no forward progress to the next prompt: {prompt_error}"
            ),
            recent_console: self.console_tail(),
        }
    }

    fn xmodem_error(
        &self,
        xmodem_error: &crate::xmodem::XmodemError,
        stage: TransferStage,
    ) -> UnbrkError {
        UnbrkError::Xmodem {
            stage,
            detail: xmodem_error.to_string(),
            recent_console: self.console_tail(),
        }
    }

    fn console_tail(&self) -> ConsoleTail {
        ConsoleTail::from_buffer(&self.console)
    }
}

fn read_image(path: &Path, image: ImageKind) -> Result<Vec<u8>, UnbrkError> {
    fs::read(path).map_err(|error| UnbrkError::BadInput {
        message: format!(
            "failed to read {image} image at {}: {error}",
            path.display()
        ),
    })
}

fn prepare_stage_payloads(plan: &FlashPlan) -> Result<Vec<PreparedStage>, UnbrkError> {
    let mut prepared_stages = Vec::with_capacity(plan.write_stages.len());

    for stage in &plan.write_stages {
        let payload = read_image(stage.image_path.as_path(), stage.image)?;
        stage.validate_image_size(
            plan.block_size,
            u64::try_from(payload.len()).unwrap_or(u64::MAX),
        )?;
        prepared_stages.push(PreparedStage {
            stage: stage.clone(),
            payload,
        });
    }

    Ok(prepared_stages)
}

fn payload_block_count(block_size: crate::target::MmcBlockSize, payload: &[u8]) -> BlockCount {
    let payload_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    let block_bytes = u64::from(block_size.get());
    let blocks = payload_bytes.div_ceil(block_bytes);

    BlockCount::new(u32::try_from(blocks).unwrap_or(u32::MAX))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{FlashConfig, flash_from_uboot, payload_block_count};
    use crate::error::UnbrkError;
    use crate::event::{Event, EventPayload, ImageKind, TransferStage};
    use crate::target::{AN7581, BlockCount};
    use crate::transport::{MockStep, MockTransport, Transport};
    use crate::uboot::{LoadAddr, UBootCommandOutput};
    use crate::xmodem::{XMODEM_ACK, XMODEM_NAK, XmodemConfig, build_crc_packet};
    use std::fs::{self, File};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
    const RESET_TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn flash_sequence_executes_happy_path_and_reports_reset_evidence() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = scripted_flash_transport(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![XMODEM_ACK],
            fixture_reset_evidence(),
        );

        let report = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap();

        assert_eq!(report.loadaddr, LoadAddr::new(0x8180_0000));
        assert_eq!(report.reset_evidence, "EcoNet System Reset");
        assert!(!report.preloader_recovered_from_eot_quirk);
        assert!(!report.fip_recovered_from_eot_quirk);
        assert!(report.events.iter().any(|event| matches!(
            event.payload,
            EventPayload::ImageVerified {
                image: ImageKind::Preloader,
                expected_size_bytes: 4,
                observed_size_bytes: 4,
            }
        )));
        assert!(
            report
                .events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::ResetSeen { .. }))
        );
        assert!(report.events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UBootCommandStarted { command } if command == "reset"
        )));
        transport.assert_finished();
    }

    #[test]
    fn payload_block_count_rounds_up_partial_blocks() {
        let tiny_payload = [0x11_u8; 4];
        let slightly_over_block = vec![0x22_u8; 513];

        assert_eq!(
            payload_block_count(AN7581.flash.block_size, &tiny_payload),
            BlockCount::new(1)
        );
        assert_eq!(
            payload_block_count(AN7581.flash.block_size, &slightly_over_block),
            BlockCount::new(2)
        );
    }

    #[test]
    fn reports_when_loadx_total_size_summary_is_absent() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = scripted_flash_transport_with_loadx_outputs(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![XMODEM_ACK],
            b"\r\nAN7581> ".to_vec(),
            b"\r\nAN7581> ".to_vec(),
            fixture_reset_evidence(),
        );

        let report = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap();

        assert!(report.events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UBootCommandCompleted { command, summary, .. }
                if command == "loadx $loadaddr 115200"
                    && summary.as_deref() == Some("loadx completed; Total Size summary absent")
        )));
        transport.assert_finished();
    }

    #[test]
    fn malformed_total_size_summary_is_a_protocol_error() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = scripted_flash_transport_with_loadx_outputs(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![XMODEM_ACK],
            b"\r\nTotal Size = nope\r\nAN7581> ".to_vec(),
            b"\r\nTotal Size = 0x4 = 4 Bytes\r\nAN7581> ".to_vec(),
            fixture_reset_evidence(),
        );

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to parse loadx total size")
        );
    }

    #[test]
    fn total_size_verification_reports_the_hex_count_when_only_hex_disagrees() {
        let output = UBootCommandOutput::new(b"Total Size = 0x5 = 4 Bytes\r\nAN7581> ".to_vec());

        let error = super::FlashRunner::<MockTransport, fn(&Event)>::verify_total_size(
            ImageKind::Preloader,
            &[0x11, 0x22, 0x33, 0x44],
            &output,
        )
        .unwrap_err();

        match error {
            UnbrkError::VerificationMismatch {
                image,
                expected_bytes,
                observed_bytes,
                recent_console,
            } => {
                assert_eq!(image, ImageKind::Preloader);
                assert_eq!(expected_bytes, 4);
                assert_eq!(observed_bytes, 5);
                assert!(recent_console.as_lossy_str().contains("0x5 = 4 Bytes"));
            }
            other => panic!("expected verification mismatch, got {other:?}"),
        }
    }

    #[test]
    fn total_size_verification_reports_the_decimal_count_when_only_decimal_disagrees() {
        let output = UBootCommandOutput::new(b"Total Size = 0x4 = 5 Bytes\r\nAN7581> ".to_vec());

        let error = super::FlashRunner::<MockTransport, fn(&Event)>::verify_total_size(
            ImageKind::Fip,
            &[0xaa, 0xbb, 0xcc, 0xdd],
            &output,
        )
        .unwrap_err();

        match error {
            UnbrkError::VerificationMismatch {
                image,
                expected_bytes,
                observed_bytes,
                recent_console,
            } => {
                assert_eq!(image, ImageKind::Fip);
                assert_eq!(expected_bytes, 4);
                assert_eq!(observed_bytes, 5);
                assert!(recent_console.as_lossy_str().contains("0x4 = 5 Bytes"));
            }
            other => panic!("expected verification mismatch, got {other:?}"),
        }
    }

    #[test]
    fn validates_images_before_erasing_flash() {
        let preloader = temp_file_with_size(129_025);
        let fip = temp_file_with_size(4);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = MockTransport::new([]);

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, crate::error::UnbrkError::BadInput { .. }));
        assert!(transport.writes().is_empty());
    }

    #[test]
    fn missing_images_fail_during_read_before_erasing_flash() {
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let missing_preloader = unique_temp_path();
        let plan = AN7581.flash_plan(missing_preloader, fip.path.clone());
        let mut transport = MockTransport::new([]);

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap_err();

        match error {
            crate::error::UnbrkError::BadInput { message } => {
                assert!(message.contains("failed to read preloader image"));
                assert!(!message.contains("failed to inspect preloader image"));
            }
            other => panic!("expected a bad input error, got {other:?}"),
        }
        assert!(transport.writes().is_empty());
    }

    #[test]
    fn tolerates_loadx_eot_quirk_when_prompt_reappears() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = scripted_flash_transport(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![XMODEM_NAK],
            b"resetting ...\r\nEcoNet System Reset\r\n".to_vec(),
        );

        let report = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(
                COMMAND_TIMEOUT,
                RESET_TIMEOUT,
                XmodemConfig::new(Duration::ZERO, 10, 1),
            ),
            |_| {},
        )
        .unwrap();

        assert!(report.preloader_recovered_from_eot_quirk);
        assert!(report.events.iter().any(|event| matches!(
            event.payload,
            EventPayload::XmodemCompleted {
                stage: TransferStage::LoadxPreloader,
                recovered_from_eot_quirk: true,
                ..
            }
        )));
        transport.assert_finished();
    }

    #[test]
    fn loadx_cancel_does_not_recover_to_the_next_prompt() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = scripted_flash_transport(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![crate::xmodem::XMODEM_CAN],
            fixture_reset_evidence(),
        );

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(
                COMMAND_TIMEOUT,
                RESET_TIMEOUT,
                XmodemConfig::new(Duration::ZERO, 10, 1),
            ),
            |_| {},
        )
        .unwrap_err();

        match error {
            crate::error::UnbrkError::Xmodem { stage, detail, .. } => {
                assert_eq!(stage, TransferStage::LoadxPreloader);
                assert!(detail.contains("receiver cancelled"));
            }
            other => panic!("expected an XMODEM error, got {other:?}"),
        }
        assert!(!transport.is_finished());
    }

    #[test]
    fn loadx_block_ack_timeout_does_not_recover_to_the_next_prompt() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut transport = MockTransport::new([
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(vec![b'\r']),
            MockStep::Flush,
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"\r\nAN7581> ".to_vec()),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"printenv loadaddr\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> printenv loadaddr\r\nloadaddr=0x81800000\r\nAN7581> ".to_vec(),
            ),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"mmc erase 0x0 0x800\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> mmc erase 0x0 0x800\r\n2048 blocks erased: OK\r\nAN7581> ".to_vec(),
            ),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"loadx $loadaddr 115200\n".to_vec()),
            MockStep::Flush,
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"loadx $loadaddr 115200\r\nCCC".to_vec()),
            MockStep::Write(build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44])),
            MockStep::Flush,
            MockStep::ReadError {
                kind: io::ErrorKind::TimedOut,
                message: String::from("block ack timed out"),
            },
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"\r\nAN7581> ".to_vec()),
        ]);

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap_err();

        match error {
            crate::error::UnbrkError::Xmodem { stage, detail, .. } => {
                assert_eq!(stage, TransferStage::LoadxPreloader);
                assert!(detail.contains("timed out while waiting for block ACK/NAK"));
            }
            other => panic!("expected an XMODEM error, got {other:?}"),
        }
        assert!(!transport.is_finished());
    }

    #[test]
    fn preloads_images_before_erasing_flash() {
        let preloader = temp_file_with_bytes(&[0x11, 0x22, 0x33, 0x44]);
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let transport = scripted_flash_transport(
            build_crc_packet(1, &[0x11, 0x22, 0x33, 0x44]),
            build_crc_packet(1, &[0xaa, 0xbb, 0xcc, 0xdd]),
            vec![XMODEM_ACK],
            fixture_reset_evidence(),
        );
        let mut transport = EraseDeletesFileTransport::new(
            transport,
            preloader.path.clone(),
            b"mmc erase 0x0 0x800\n".to_vec(),
        );

        let report = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap();

        assert_eq!(report.reset_evidence, "EcoNet System Reset");
        assert!(!preloader.path.exists());
        transport.assert_finished();
    }

    #[test]
    fn fails_before_erasing_when_image_cannot_be_read() {
        let preloader = unique_temp_path();
        let fip = temp_file_with_bytes(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let plan = AN7581.flash_plan(preloader, fip.path.clone());
        let mut transport = MockTransport::new([]);

        let error = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, XmodemConfig::default()),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, crate::error::UnbrkError::BadInput { .. }));
        assert!(transport.writes().is_empty());
    }

    fn scripted_flash_transport(
        preloader_packet: Vec<u8>,
        fip_packet: Vec<u8>,
        preloader_eot_reply: Vec<u8>,
        reset_output: Vec<u8>,
    ) -> MockTransport {
        scripted_flash_transport_with_loadx_outputs(
            preloader_packet,
            fip_packet,
            preloader_eot_reply,
            b"\r\nTotal Size = 0x4 = 4 Bytes\r\nAN7581> ".to_vec(),
            b"\r\nTotal Size = 0x4 = 4 Bytes\r\nAN7581> ".to_vec(),
            reset_output,
        )
    }

    fn scripted_flash_transport_with_loadx_outputs(
        preloader_packet: Vec<u8>,
        fip_packet: Vec<u8>,
        preloader_eot_reply: Vec<u8>,
        preloader_loadx_output: Vec<u8>,
        fip_loadx_output: Vec<u8>,
        reset_output: Vec<u8>,
    ) -> MockTransport {
        MockTransport::new([
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(vec![b'\r']),
            MockStep::Flush,
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"\r\nAN7581> ".to_vec()),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"printenv loadaddr\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> printenv loadaddr\r\nloadaddr=0x81800000\r\nAN7581> ".to_vec(),
            ),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"mmc erase 0x0 0x800\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> mmc erase 0x0 0x800\r\n2048 blocks erased: OK\r\nAN7581> ".to_vec(),
            ),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"loadx $loadaddr 115200\n".to_vec()),
            MockStep::Flush,
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"loadx $loadaddr 115200\r\nCCC".to_vec()),
            MockStep::Write(preloader_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(preloader_eot_reply),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(preloader_loadx_output),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"printenv filesize\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(b"AN7581> printenv filesize\r\nfilesize=4\r\nAN7581> ".to_vec()),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"mmc write $loadaddr 0x4 0x1\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> mmc write $loadaddr 0x4 0x1\r\n1 blocks written: OK\r\nAN7581> ".to_vec(),
            ),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"loadx $loadaddr 115200\n".to_vec()),
            MockStep::Flush,
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(b"loadx $loadaddr 115200\r\nCCC".to_vec()),
            MockStep::Write(fip_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Read(fip_loadx_output),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"printenv filesize\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(b"AN7581> printenv filesize\r\nfilesize=0x4\r\nAN7581> ".to_vec()),
            MockStep::SetTimeout(COMMAND_TIMEOUT),
            MockStep::Write(b"mmc write $loadaddr 0x100 0x1\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(
                b"AN7581> mmc write $loadaddr 0x100 0x1\r\n1 blocks written: OK\r\nAN7581> "
                    .to_vec(),
            ),
            MockStep::SetTimeout(RESET_TIMEOUT),
            MockStep::Write(b"reset\n".to_vec()),
            MockStep::Flush,
            MockStep::Read(reset_output),
        ])
    }

    fn fixture_reset_evidence() -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/an7581/reset-evidence.bin"),
        )
        .unwrap()
    }

    fn temp_file_with_bytes(bytes: &[u8]) -> TempFile {
        let path = unique_temp_path();
        fs::write(&path, bytes).unwrap();

        TempFile { path }
    }

    fn temp_file_with_size(size: u64) -> TempFile {
        let path = unique_temp_path();
        let file = File::create(&path).unwrap();
        file.set_len(size).unwrap();

        TempFile { path }
    }

    fn unique_temp_path() -> PathBuf {
        let unique_id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "unbrk-flash-tests-{}-{unique_id}.bin",
            std::process::id()
        ))
    }

    struct TempFile {
        path: PathBuf,
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ignored = fs::remove_file(&self.path);
        }
    }

    struct EraseDeletesFileTransport {
        inner: MockTransport,
        path_to_delete: PathBuf,
        erase_command: Vec<u8>,
        deleted: bool,
    }

    impl EraseDeletesFileTransport {
        fn new(inner: MockTransport, path_to_delete: PathBuf, erase_command: Vec<u8>) -> Self {
            Self {
                inner,
                path_to_delete,
                erase_command,
                deleted: false,
            }
        }

        fn assert_finished(&self) {
            self.inner.assert_finished();
        }
    }

    impl Transport for EraseDeletesFileTransport {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buffer)
        }

        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            if !self.deleted && bytes == self.erase_command {
                fs::remove_file(&self.path_to_delete)?;
                self.deleted = true;
            }
            self.inner.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }

        fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.inner.set_timeout(timeout)
        }
    }
}
