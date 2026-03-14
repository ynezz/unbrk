//! Structured recovery events.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

/// First stable schema version for the shared recovery event stream.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Recovery event wrapper with monotonic ordering metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// Creates an event with an explicit timestamp.
    #[must_use]
    pub const fn new(sequence: u64, timestamp_unix_ms: u64, payload: EventPayload) -> Self {
        Self {
            sequence,
            timestamp_unix_ms,
            payload,
        }
    }

    /// Creates an event timestamped with the current system clock.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock is earlier than the Unix epoch.
    pub fn now(sequence: u64, payload: EventPayload) -> Result<Self, SystemTimeError> {
        Ok(Self::new(sequence, timestamp_now_unix_ms()?, payload))
    }

    /// Returns the stable kind identifier for this event.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        self.payload.kind()
    }
}

impl fmt::Display for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{} {}", self.sequence, self.payload)
    }
}

/// Structured event payload shared by human and machine output modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    SessionStarted {
        schema_version: u32,
        tool_version: String,
        target_profile: String,
        serial_port: Option<String>,
    },
    PortOpened {
        port: String,
        baud: u32,
    },
    PromptSeen {
        stage: RecoveryStage,
        prompt: String,
    },
    InputSent {
        stage: RecoveryStage,
        input: String,
    },
    CrcReady {
        stage: TransferStage,
        readiness_bytes_seen: u32,
    },
    XmodemStarted {
        stage: TransferStage,
        file_name: String,
        size_bytes: u64,
    },
    XmodemProgress {
        stage: TransferStage,
        bytes_sent: u64,
        total_bytes: u64,
    },
    XmodemCompleted {
        stage: TransferStage,
        bytes_sent: u64,
        expected_bytes: u64,
        recovered_from_eot_quirk: bool,
    },
    UBootPromptSeen {
        prompt: String,
    },
    UBootCommandStarted {
        command: String,
    },
    UBootCommandCompleted {
        command: String,
        success: bool,
        summary: Option<String>,
    },
    ImageVerified {
        image: ImageKind,
        expected_size_bytes: u64,
        observed_size_bytes: u64,
    },
    ResetSeen {
        evidence: String,
    },
    HandoffReady {
        interactive_console: bool,
    },
    Failure {
        class: FailureClass,
        message: String,
    },
}

impl EventPayload {
    /// Returns the stable kind identifier for this payload.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::SessionStarted { .. } => EventKind::SessionStarted,
            Self::PortOpened { .. } => EventKind::PortOpened,
            Self::PromptSeen { .. } => EventKind::PromptSeen,
            Self::InputSent { .. } => EventKind::InputSent,
            Self::CrcReady { .. } => EventKind::CrcReady,
            Self::XmodemStarted { .. } => EventKind::XmodemStarted,
            Self::XmodemProgress { .. } => EventKind::XmodemProgress,
            Self::XmodemCompleted { .. } => EventKind::XmodemCompleted,
            Self::UBootPromptSeen { .. } => EventKind::UBootPromptSeen,
            Self::UBootCommandStarted { .. } => EventKind::UBootCommandStarted,
            Self::UBootCommandCompleted { .. } => EventKind::UBootCommandCompleted,
            Self::ImageVerified { .. } => EventKind::ImageVerified,
            Self::ResetSeen { .. } => EventKind::ResetSeen,
            Self::HandoffReady { .. } => EventKind::HandoffReady,
            Self::Failure { .. } => EventKind::Failure,
        }
    }

    fn fmt_xmodem_completed(
        formatter: &mut fmt::Formatter<'_>,
        stage: TransferStage,
        bytes_sent: u64,
        expected_bytes: u64,
        recovered_from_eot_quirk: bool,
    ) -> fmt::Result {
        write!(
            formatter,
            "{stage} XMODEM completed with {bytes_sent}/{expected_bytes} bytes",
        )?;

        if recovered_from_eot_quirk {
            formatter.write_str(" after tolerating an EOT quirk")?;
        }

        Ok(())
    }

    fn fmt_uboot_command_completed(
        formatter: &mut fmt::Formatter<'_>,
        command: &str,
        success: bool,
        summary: Option<&str>,
    ) -> fmt::Result {
        write!(
            formatter,
            "completed U-Boot command: {command} ({})",
            if success { "ok" } else { "failed" }
        )?;

        if let Some(summary) = summary {
            write!(formatter, " - {summary}")?;
        }

        Ok(())
    }
}

impl fmt::Display for EventPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionStarted {
                schema_version,
                tool_version,
                target_profile,
                serial_port,
            } => write!(
                formatter,
                "session started: schema v{schema_version}, tool {tool_version}, target {target_profile}, port {}",
                serial_port.as_deref().unwrap_or("auto"),
            ),
            Self::PortOpened { port, baud } => {
                write!(formatter, "opened serial port {port} at {baud} baud")
            }
            Self::PromptSeen { stage, prompt } => {
                write!(formatter, "{stage} prompt seen: {prompt}")
            }
            Self::InputSent { stage, input } => {
                write!(formatter, "sent input {input:?} during {stage}")
            }
            Self::CrcReady {
                stage,
                readiness_bytes_seen,
            } => write!(
                formatter,
                "{stage} XMODEM CRC ready after {readiness_bytes_seen} readiness byte(s)"
            ),
            Self::XmodemStarted {
                stage,
                file_name,
                size_bytes,
            } => write!(
                formatter,
                "started {stage} XMODEM transfer for {file_name} ({size_bytes} bytes)"
            ),
            Self::XmodemProgress {
                stage,
                bytes_sent,
                total_bytes,
            } => write!(
                formatter,
                "{stage} XMODEM progress: {bytes_sent}/{total_bytes} bytes"
            ),
            Self::XmodemCompleted {
                stage,
                bytes_sent,
                expected_bytes,
                recovered_from_eot_quirk,
            } => Self::fmt_xmodem_completed(
                formatter,
                *stage,
                *bytes_sent,
                *expected_bytes,
                *recovered_from_eot_quirk,
            ),
            Self::UBootPromptSeen { prompt } => {
                write!(formatter, "U-Boot prompt seen: {prompt}")
            }
            Self::UBootCommandStarted { command } => {
                write!(formatter, "started U-Boot command: {command}")
            }
            Self::UBootCommandCompleted {
                command,
                success,
                summary,
            } => {
                Self::fmt_uboot_command_completed(formatter, command, *success, summary.as_deref())
            }
            Self::ImageVerified {
                image,
                expected_size_bytes,
                observed_size_bytes,
            } => write!(
                formatter,
                "verified {image}: expected {expected_size_bytes} bytes, observed {observed_size_bytes} bytes"
            ),
            Self::ResetSeen { evidence } => write!(formatter, "reset observed: {evidence}"),
            Self::HandoffReady {
                interactive_console,
            } => write!(
                formatter,
                "handoff ready: {}",
                if *interactive_console {
                    "interactive console enabled"
                } else {
                    "machine-controlled stop point"
                }
            ),
            Self::Failure { class, message } => {
                write!(formatter, "{class} failure: {message}")
            }
        }
    }
}

/// Stable event-kind identifier for routing and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    PortOpened,
    PromptSeen,
    InputSent,
    CrcReady,
    XmodemStarted,
    XmodemProgress,
    XmodemCompleted,
    UBootPromptSeen,
    UBootCommandStarted,
    UBootCommandCompleted,
    ImageVerified,
    ResetSeen,
    HandoffReady,
    Failure,
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SessionStarted => "session_started",
            Self::PortOpened => "port_opened",
            Self::PromptSeen => "prompt_seen",
            Self::InputSent => "input_sent",
            Self::CrcReady => "crc_ready",
            Self::XmodemStarted => "xmodem_started",
            Self::XmodemProgress => "xmodem_progress",
            Self::XmodemCompleted => "xmodem_completed",
            Self::UBootPromptSeen => "uboot_prompt_seen",
            Self::UBootCommandStarted => "uboot_command_started",
            Self::UBootCommandCompleted => "uboot_command_completed",
            Self::ImageVerified => "image_verified",
            Self::ResetSeen => "reset_seen",
            Self::HandoffReady => "handoff_ready",
            Self::Failure => "failure",
        })
    }
}

/// Recovery stage used by prompt and input events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStage {
    Bootrom,
    PreloaderPrompt,
    FipPrompt,
    UBoot,
    FlashPlan,
}

impl fmt::Display for RecoveryStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bootrom => "bootrom",
            Self::PreloaderPrompt => "preloader prompt",
            Self::FipPrompt => "FIP prompt",
            Self::UBoot => "U-Boot",
            Self::FlashPlan => "flash plan",
        })
    }
}

/// Transfer stage used by CRC and XMODEM events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStage {
    Preloader,
    Fip,
    LoadxPreloader,
    LoadxFip,
}

impl fmt::Display for TransferStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Preloader => "preloader",
            Self::Fip => "FIP",
            Self::LoadxPreloader => "loadx preloader",
            Self::LoadxFip => "loadx FIP",
        })
    }
}

/// Image identity used by verification events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageKind {
    Preloader,
    Fip,
}

impl fmt::Display for ImageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Preloader => "preloader",
            Self::Fip => "FIP",
        })
    }
}

/// Stable failure classes shared with the CLI exit-code contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Serial,
    Timeout,
    Protocol,
    Xmodem,
    UBootCommand,
    VerificationMismatch,
    BadInput,
    UserAbort,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Serial => "serial",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::Xmodem => "xmodem",
            Self::UBootCommand => "u_boot_command",
            Self::VerificationMismatch => "verification_mismatch",
            Self::BadInput => "bad_input",
            Self::UserAbort => "user_abort",
        })
    }
}

fn timestamp_now_unix_ms() -> Result<u64, SystemTimeError> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{
        EVENT_SCHEMA_VERSION, Event, EventKind, EventPayload, FailureClass, ImageKind,
        RecoveryStage, TransferStage,
    };

    #[test]
    fn event_serializes_with_a_kind_tag_and_payload_fields() {
        let event = Event::new(
            1,
            1234,
            EventPayload::SessionStarted {
                schema_version: EVENT_SCHEMA_VERSION,
                tool_version: String::from("0.1.0"),
                target_profile: String::from("valyrian"),
                serial_port: Some(String::from("/dev/ttyUSB0")),
            },
        );

        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["sequence"], 1);
        assert_eq!(json["timestamp_unix_ms"], 1234);
        assert_eq!(json["kind"], "session_started");
        assert_eq!(json["schema_version"], EVENT_SCHEMA_VERSION);
        assert_eq!(json["target_profile"], "valyrian");
        assert_eq!(json["serial_port"], "/dev/ttyUSB0");
    }

    #[test]
    fn kind_method_matches_the_payload_kind() {
        let event = Event::new(
            7,
            42,
            EventPayload::ImageVerified {
                image: ImageKind::Fip,
                expected_size_bytes: 100,
                observed_size_bytes: 100,
            },
        );

        assert_eq!(event.kind(), EventKind::ImageVerified);
    }

    #[test]
    fn display_is_human_readable() {
        let event = Event::new(
            2,
            999,
            EventPayload::PromptSeen {
                stage: RecoveryStage::FipPrompt,
                prompt: String::from("Press x to load BL31 + U-Boot FIP"),
            },
        );

        assert_eq!(
            event.to_string(),
            "#2 FIP prompt prompt seen: Press x to load BL31 + U-Boot FIP"
        );
    }

    #[test]
    fn failure_event_renders_its_failure_class() {
        let payload = EventPayload::Failure {
            class: FailureClass::Protocol,
            message: String::from("unexpected prompt"),
        };

        assert_eq!(payload.to_string(), "protocol failure: unexpected prompt");
    }

    #[test]
    fn xmodem_completion_mentions_eot_recovery_when_present() {
        let payload = EventPayload::XmodemCompleted {
            stage: TransferStage::Preloader,
            bytes_sent: 1024,
            expected_bytes: 1024,
            recovered_from_eot_quirk: true,
        };

        assert_eq!(
            payload.to_string(),
            "preloader XMODEM completed with 1024/1024 bytes after tolerating an EOT quirk"
        );
    }
}
