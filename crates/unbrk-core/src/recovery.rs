//! Recovery-state orchestration for reaching the RAM-resident U-Boot prompt.

use crate::error::{ConsoleTail, UnbrkError};
use crate::event::{Event, EventPayload, RecoveryStage, TransferStage};
use crate::prompt::{
    PromptMatch, advance_to_prompt_allowing_trailing_space_with_regex, advance_to_prompt_with_regex,
};
use crate::target::{PromptPattern, TargetProfile};
use crate::transport::Transport;
use crate::xmodem::{
    CrcReadyMatch, XmodemConfig, XmodemTransferReport, advance_to_crc_ready, send_crc,
};
use std::time::Duration;

/// Default prompt timeout for each recovery state.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Payloads required for the two recovery transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryImages<'a> {
    pub preloader_name: &'a str,
    pub preloader: &'a [u8],
    pub fip_name: &'a str,
    pub fip: &'a [u8],
}

/// Tunables for the recovery state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryConfig {
    pub prompt_timeout: Duration,
    pub xmodem: XmodemConfig,
}

impl RecoveryConfig {
    #[must_use]
    pub const fn new(prompt_timeout: Duration, xmodem: XmodemConfig) -> Self {
        Self {
            prompt_timeout,
            xmodem,
        }
    }
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self::new(DEFAULT_PROMPT_TIMEOUT, XmodemConfig::default())
    }
}

/// Explicit recovery states traversed before the U-Boot prompt is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    WaitForInitialPrompt,
    SendXForPreloader,
    WaitForXmodemCrcPreloader,
    SendPreloader,
    WaitForFipPrompt,
    SendXForFip,
    WaitForXmodemCrcFip,
    SendFip,
    WaitForUbootPrompt,
    Complete,
}

/// End-to-end recovery outcome with emitted events and visited states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    pub states: Vec<RecoveryState>,
    pub events: Vec<Event>,
    pub console: Vec<u8>,
    pub preloader_recovered_from_eot_quirk: bool,
    pub fip_recovered_from_eot_quirk: bool,
}

/// Reaches the RAM-resident U-Boot prompt using the documented recovery flow.
///
/// # Errors
///
/// Returns a typed recovery error when prompt waits, CRC readiness detection,
/// or XMODEM transfers fail without forward progress to the next prompt.
pub fn recover_to_uboot(
    transport: &mut impl Transport,
    target: TargetProfile,
    images: RecoveryImages<'_>,
    config: RecoveryConfig,
) -> Result<RecoveryReport, UnbrkError> {
    let mut runner = RecoveryRunner::new(transport, target, config);

    runner.push_state(RecoveryState::WaitForInitialPrompt);
    let initial_prompt = runner.read_stage_prompt(
        target.prompts.initial_recovery,
        RecoveryStage::PreloaderPrompt,
        "the initial recovery prompt",
    )?;
    runner.emit(EventPayload::PromptSeen {
        stage: RecoveryStage::PreloaderPrompt,
        prompt: initial_prompt.prompt,
    });

    runner.push_state(RecoveryState::SendXForPreloader);
    runner.send_literal_input(RecoveryStage::PreloaderPrompt, b'x')?;

    runner.push_state(RecoveryState::WaitForXmodemCrcPreloader);
    let preloader_crc = runner.read_crc_ready(
        RecoveryStage::PreloaderPrompt,
        "preloader XMODEM CRC readiness",
    )?;
    runner.emit(EventPayload::CrcReady {
        stage: TransferStage::Preloader,
        readiness_bytes_seen: preloader_crc.readiness_bytes_seen,
    });

    runner.push_state(RecoveryState::SendPreloader);
    runner.emit(EventPayload::XmodemStarted {
        stage: TransferStage::Preloader,
        file_name: images.preloader_name.to_owned(),
        size_bytes: u64::try_from(images.preloader.len()).unwrap_or(u64::MAX),
    });
    let preloader_result = runner.send_transfer(
        TransferStage::Preloader,
        images.preloader,
        target.prompts.second_stage,
        RecoveryState::WaitForFipPrompt,
        WaitTarget::RecoveryPrompt {
            stage: RecoveryStage::FipPrompt,
            operation: "the BL31 + U-Boot FIP prompt",
            event: EventPayload::PromptSeen {
                stage: RecoveryStage::FipPrompt,
                prompt: String::new(),
            },
        },
    )?;

    if !preloader_result.prompt_emitted {
        runner.push_state(RecoveryState::WaitForFipPrompt);
        let second_prompt = runner.read_stage_prompt(
            target.prompts.second_stage,
            RecoveryStage::FipPrompt,
            "the BL31 + U-Boot FIP prompt",
        )?;
        runner.emit(EventPayload::PromptSeen {
            stage: RecoveryStage::FipPrompt,
            prompt: second_prompt.prompt,
        });
    }

    runner.push_state(RecoveryState::SendXForFip);
    runner.send_literal_input(RecoveryStage::FipPrompt, b'x')?;

    runner.push_state(RecoveryState::WaitForXmodemCrcFip);
    let fip_crc = runner.read_crc_ready(RecoveryStage::FipPrompt, "FIP XMODEM CRC readiness")?;
    runner.emit(EventPayload::CrcReady {
        stage: TransferStage::Fip,
        readiness_bytes_seen: fip_crc.readiness_bytes_seen,
    });

    runner.push_state(RecoveryState::SendFip);
    runner.emit(EventPayload::XmodemStarted {
        stage: TransferStage::Fip,
        file_name: images.fip_name.to_owned(),
        size_bytes: u64::try_from(images.fip.len()).unwrap_or(u64::MAX),
    });
    let fip_result = runner.send_transfer(
        TransferStage::Fip,
        images.fip,
        target.prompts.uboot,
        RecoveryState::WaitForUbootPrompt,
        WaitTarget::UBootPrompt {
            operation: "the RAM-resident U-Boot prompt",
        },
    )?;

    if !fip_result.prompt_emitted {
        runner.push_state(RecoveryState::WaitForUbootPrompt);
        let uboot_prompt =
            runner.read_uboot_prompt(target.prompts.uboot, "the RAM-resident U-Boot prompt")?;
        runner.emit(EventPayload::UBootPromptSeen {
            prompt: uboot_prompt.prompt,
        });
    }

    runner.push_state(RecoveryState::Complete);

    Ok(RecoveryReport {
        states: runner.states,
        events: runner.events,
        console: runner.console,
        preloader_recovered_from_eot_quirk: preloader_result.recovered_from_eot_quirk,
        fip_recovered_from_eot_quirk: fip_result.recovered_from_eot_quirk,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransferOutcome {
    recovered_from_eot_quirk: bool,
    prompt_emitted: bool,
}

enum WaitTarget {
    RecoveryPrompt {
        stage: RecoveryStage,
        operation: &'static str,
        event: EventPayload,
    },
    UBootPrompt {
        operation: &'static str,
    },
}

struct RecoveryRunner<'a, T> {
    transport: &'a mut T,
    config: RecoveryConfig,
    console: Vec<u8>,
    cursor: usize,
    events: Vec<Event>,
    states: Vec<RecoveryState>,
    next_sequence: u64,
}

impl<'a, T: Transport> RecoveryRunner<'a, T> {
    const fn new(transport: &'a mut T, _target: TargetProfile, config: RecoveryConfig) -> Self {
        Self {
            transport,
            config,
            console: Vec::new(),
            cursor: 0,
            events: Vec::new(),
            states: Vec::new(),
            next_sequence: 1,
        }
    }

    fn push_state(&mut self, state: RecoveryState) {
        self.states.push(state);
    }

    fn emit(&mut self, payload: EventPayload) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let event = Event::now(sequence, payload.clone())
            .unwrap_or_else(|_| Event::new(sequence, 0, payload));
        self.events.push(event);
    }

    fn send_literal_input(&mut self, stage: RecoveryStage, byte: u8) -> Result<(), UnbrkError> {
        self.transport
            .write_byte(byte)
            .map_err(|source| UnbrkError::Serial {
                operation: "writing a recovery-mode input byte",
                source,
            })?;
        self.emit(EventPayload::InputSent {
            stage,
            input: char::from(byte).to_string(),
        });
        Ok(())
    }

    fn send_transfer(
        &mut self,
        stage: TransferStage,
        payload: &[u8],
        prompt_pattern: PromptPattern,
        wait_state: RecoveryState,
        wait_target: WaitTarget,
    ) -> Result<TransferOutcome, UnbrkError> {
        let mut progress_events = Vec::new();
        let transfer = send_crc(
            self.transport,
            stage,
            payload,
            self.config.xmodem,
            |progress| progress_events.push(progress),
        );

        for progress in progress_events {
            self.emit(EventPayload::XmodemProgress {
                stage: progress.stage,
                bytes_sent: progress.bytes_sent,
                total_bytes: progress.total_bytes,
            });
        }

        match transfer {
            Ok(report) => {
                self.emit_completed(report, false);
                Ok(TransferOutcome {
                    recovered_from_eot_quirk: false,
                    prompt_emitted: false,
                })
            }
            Err(error) => {
                self.push_state(wait_state);
                match wait_target {
                    WaitTarget::RecoveryPrompt {
                        stage: recovery_stage,
                        operation,
                        event,
                    } => {
                        let prompt =
                            self.read_stage_prompt(prompt_pattern, recovery_stage, operation)?;
                        self.emit_completed_from_payload(stage, payload, true);
                        if let EventPayload::PromptSeen { stage, .. } = event {
                            self.emit(EventPayload::PromptSeen {
                                stage,
                                prompt: prompt.prompt,
                            });
                        }
                        Ok(TransferOutcome {
                            recovered_from_eot_quirk: true,
                            prompt_emitted: true,
                        })
                    }
                    WaitTarget::UBootPrompt { operation } => {
                        let prompt = self.read_uboot_prompt(prompt_pattern, operation)?;
                        self.emit_completed_from_payload(stage, payload, true);
                        self.emit(EventPayload::UBootPromptSeen {
                            prompt: prompt.prompt,
                        });
                        Ok(TransferOutcome {
                            recovered_from_eot_quirk: true,
                            prompt_emitted: true,
                        })
                    }
                }
                .map_err(|prompt_error| self.xmodem_failure(&error, &prompt_error, stage))
            }
        }
    }

    fn emit_completed(&mut self, report: XmodemTransferReport, recovered_from_eot_quirk: bool) {
        self.emit(EventPayload::XmodemCompleted {
            stage: report.stage,
            bytes_sent: report.bytes_sent,
            expected_bytes: report.total_bytes,
            recovered_from_eot_quirk,
        });
    }

    fn emit_completed_from_payload(
        &mut self,
        stage: TransferStage,
        payload: &[u8],
        recovered_from_eot_quirk: bool,
    ) {
        let size = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        self.emit(EventPayload::XmodemCompleted {
            stage,
            bytes_sent: size,
            expected_bytes: size,
            recovered_from_eot_quirk,
        });
    }

    fn read_stage_prompt(
        &mut self,
        pattern: PromptPattern,
        stage: RecoveryStage,
        operation: &'static str,
    ) -> Result<PromptMatch, UnbrkError> {
        let regex = pattern
            .compile()
            .map_err(|error| Self::invalid_prompt_regex(&error))?;
        self.transport
            .set_timeout(self.config.prompt_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "configuring the recovery prompt timeout",
                source,
            })?;

        loop {
            if let Some(prompt) =
                advance_to_prompt_with_regex(&regex, &self.console, &mut self.cursor)
            {
                return Ok(prompt);
            }

            self.read_more(stage, operation)?;
        }
    }

    fn read_uboot_prompt(
        &mut self,
        pattern: PromptPattern,
        operation: &'static str,
    ) -> Result<PromptMatch, UnbrkError> {
        let regex = pattern
            .compile()
            .map_err(|error| Self::invalid_prompt_regex(&error))?;
        self.transport
            .set_timeout(self.config.prompt_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "configuring the recovery prompt timeout",
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

            self.read_more(RecoveryStage::UBoot, operation)?;
        }
    }

    fn read_crc_ready(
        &mut self,
        stage: RecoveryStage,
        operation: &'static str,
    ) -> Result<CrcReadyMatch, UnbrkError> {
        self.transport
            .set_timeout(self.config.prompt_timeout)
            .map_err(|source| UnbrkError::Serial {
                operation: "configuring the recovery prompt timeout",
                source,
            })?;

        loop {
            if let Some(readiness) = advance_to_crc_ready(&self.console, &mut self.cursor) {
                return Ok(readiness);
            }

            self.read_more(stage, operation)?;
        }
    }

    fn read_more(
        &mut self,
        stage: RecoveryStage,
        operation: &'static str,
    ) -> Result<(), UnbrkError> {
        let mut scratch = [0_u8; 256];
        match self.transport.read(&mut scratch) {
            Ok(0) => Err(UnbrkError::Timeout {
                stage,
                operation,
                timeout: self.config.prompt_timeout,
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
                    timeout: self.config.prompt_timeout,
                    recent_console: self.console_tail(),
                })
            }
            Err(source) => Err(UnbrkError::Serial {
                operation: "reading recovery console output",
                source,
            }),
        }
    }

    fn console_tail(&self) -> ConsoleTail {
        ConsoleTail::new(self.console.clone())
    }

    fn invalid_prompt_regex(error: &regex::Error) -> UnbrkError {
        UnbrkError::Protocol {
            stage: RecoveryStage::Bootrom,
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
}

#[cfg(test)]
mod tests {
    use super::{RecoveryConfig, RecoveryImages, RecoveryState, recover_to_uboot};
    use crate::event::{EventPayload, RecoveryStage, TransferStage};
    use crate::target::AN7581;
    use crate::transport::{MockStep, MockTransport};
    use crate::xmodem::{XMODEM_ACK, XMODEM_NAK, XmodemConfig, build_crc_packet};
    use std::time::Duration;

    const PROMPT_TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn happy_path_reaches_uboot_with_ordered_states() {
        let preloader = [0x11_u8; 4];
        let fip = [0x22_u8; 4];
        let preloader_packet = build_crc_packet(1, &preloader);
        let fip_packet = build_crc_packet(1, &fip);

        let mut transport = MockTransport::new([
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"Press x\r\n".to_vec()),
            MockStep::Write(vec![b'x']),
            MockStep::Flush,
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"CCC".to_vec()),
            MockStep::Write(preloader_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"Press x to load BL31 + U-Boot FIP\r\n".to_vec()),
            MockStep::Write(vec![b'x']),
            MockStep::Flush,
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"C\x00C\rC".to_vec()),
            MockStep::Write(fip_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"AN7581> \r\n".to_vec()),
        ]);

        let report = recover_to_uboot(
            &mut transport,
            AN7581,
            RecoveryImages {
                preloader_name: "preloader.bin",
                preloader: &preloader,
                fip_name: "fip.bin",
                fip: &fip,
            },
            RecoveryConfig::new(PROMPT_TIMEOUT, XmodemConfig::default()),
        )
        .unwrap();

        assert_eq!(
            report.states,
            vec![
                RecoveryState::WaitForInitialPrompt,
                RecoveryState::SendXForPreloader,
                RecoveryState::WaitForXmodemCrcPreloader,
                RecoveryState::SendPreloader,
                RecoveryState::WaitForFipPrompt,
                RecoveryState::SendXForFip,
                RecoveryState::WaitForXmodemCrcFip,
                RecoveryState::SendFip,
                RecoveryState::WaitForUbootPrompt,
                RecoveryState::Complete,
            ]
        );
        assert!(!report.preloader_recovered_from_eot_quirk);
        assert!(!report.fip_recovered_from_eot_quirk);
        assert!(
            report
                .events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::UBootPromptSeen { .. }))
        );
        transport.assert_finished();
    }

    #[test]
    fn failed_final_eot_can_be_tolerated_when_the_next_prompt_arrives() {
        let preloader = [0x11_u8; 4];
        let fip = [0x22_u8; 4];
        let preloader_packet = build_crc_packet(1, &preloader);
        let fip_packet = build_crc_packet(1, &fip);

        let mut transport = MockTransport::new([
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"Press x\r\n".to_vec()),
            MockStep::Write(vec![b'x']),
            MockStep::Flush,
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"CCC".to_vec()),
            MockStep::Write(preloader_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_NAK]),
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"Press x to load BL31 + U-Boot FIP\r\n".to_vec()),
            MockStep::Write(vec![b'x']),
            MockStep::Flush,
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"CCC".to_vec()),
            MockStep::Write(fip_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![crate::xmodem::XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::SetTimeout(PROMPT_TIMEOUT),
            MockStep::Read(b"AN7581> \r\n".to_vec()),
        ]);

        let report = recover_to_uboot(
            &mut transport,
            AN7581,
            RecoveryImages {
                preloader_name: "preloader.bin",
                preloader: &preloader,
                fip_name: "fip.bin",
                fip: &fip,
            },
            RecoveryConfig::new(PROMPT_TIMEOUT, XmodemConfig::new(Duration::ZERO, 10, 1)),
        )
        .unwrap();

        assert!(report.preloader_recovered_from_eot_quirk);
        assert!(report.events.iter().any(|event| matches!(
            event.payload,
            EventPayload::XmodemCompleted {
                stage: TransferStage::Preloader,
                recovered_from_eot_quirk: true,
                ..
            }
        )));
        assert!(report.events.iter().any(|event| matches!(
            event.payload,
            EventPayload::PromptSeen {
                stage: RecoveryStage::FipPrompt,
                ..
            }
        )));
        transport.assert_finished();
    }
}
