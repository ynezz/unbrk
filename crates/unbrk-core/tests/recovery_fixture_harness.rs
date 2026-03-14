mod support;

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use support::recovery_fixture_harness::{FixtureRecoveryScenario, ReplayPoint};
use unbrk_core::error::UnbrkError;
use unbrk_core::target::AN7581;
use unbrk_core::xmodem::{XMODEM_NAK, XmodemConfig};
use unbrk_core::{
    EventKind, EventPayload, FlashConfig, LoadAddr, MockStep, MockTransport, RecoveryStage,
    TransferStage, flash_from_uboot,
};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn happy_path_fixtures_drive_the_recovery_state_machine() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario.run().unwrap();

    assert_eq!(
        run.report.states.as_slice(),
        FixtureRecoveryScenario::expected_states()
    );

    let event_kinds: Vec<_> = run
        .report
        .events
        .iter()
        .map(unbrk_core::Event::kind)
        .collect();
    assert_eq!(
        event_kinds,
        vec![
            EventKind::PromptSeen,
            EventKind::InputSent,
            EventKind::CrcReady,
            EventKind::XmodemStarted,
            EventKind::XmodemProgress,
            EventKind::XmodemCompleted,
            EventKind::PromptSeen,
            EventKind::InputSent,
            EventKind::CrcReady,
            EventKind::XmodemStarted,
            EventKind::XmodemProgress,
            EventKind::XmodemCompleted,
            EventKind::UBootPromptSeen,
        ]
    );

    assert!(matches!(
        &run.report.events[0].payload,
        EventPayload::PromptSeen {
            stage: RecoveryStage::PreloaderPrompt,
            prompt,
        } if prompt == "Press x"
    ));
    assert!(matches!(
        &run.report.events[2].payload,
        EventPayload::CrcReady {
            stage: TransferStage::Preloader,
            readiness_bytes_seen,
        } if *readiness_bytes_seen >= 3
    ));
    assert!(matches!(
        &run.report.events[3].payload,
        EventPayload::XmodemStarted {
            stage: TransferStage::Preloader,
            file_name,
            size_bytes,
        } if file_name == "preloader.bin" && *size_bytes == 4
    ));
    assert!(matches!(
        &run.report.events[5].payload,
        EventPayload::XmodemCompleted {
            stage: TransferStage::Preloader,
            bytes_sent,
            expected_bytes,
            recovered_from_eot_quirk,
        } if *bytes_sent == 4 && *expected_bytes == 4 && !recovered_from_eot_quirk
    ));
    assert!(matches!(
        &run.report.events[6].payload,
        EventPayload::PromptSeen {
            stage: RecoveryStage::FipPrompt,
            prompt,
        } if prompt == "Press x to load BL31 + U-Boot FIP"
    ));
    assert!(matches!(
        &run.report.events[9].payload,
        EventPayload::XmodemStarted {
            stage: TransferStage::Fip,
            file_name,
            size_bytes,
        } if file_name == "fip.bin" && *size_bytes == 4
    ));
    assert!(matches!(
        &run.report.events[11].payload,
        EventPayload::XmodemCompleted {
            stage: TransferStage::Fip,
            bytes_sent,
            expected_bytes,
            recovered_from_eot_quirk,
        } if *bytes_sent == 4 && *expected_bytes == 4 && !recovered_from_eot_quirk
    ));
    assert!(matches!(
        &run.report.events[12].payload,
        EventPayload::UBootPromptSeen { prompt } if prompt == "AN7581>"
    ));

    let console = String::from_utf8_lossy(&run.report.console);
    assert!(!run.expected_console.is_empty());
    assert!(console.starts_with("Press x\r\n"));
    assert!(console.contains("Press x to load BL31 + U-Boot FIP"));
    assert!(console.contains("U-Boot 2026.01-OpenWrt"));
    assert!(console.ends_with("AN7581> \r\nAN7581> \r\n"));
    assert!(!run.report.preloader_recovered_from_eot_quirk);
    assert!(!run.report.fip_recovered_from_eot_quirk);
    run.transport.assert_finished();
}

#[test]
fn harness_supports_targeted_failure_injection() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_overrides([(
            ReplayPoint::FipPrompt,
            MockStep::ReadError {
                kind: io::ErrorKind::TimedOut,
                message: String::from("injected timeout before the stage-two prompt"),
            },
        )])
        .unwrap_err();

    match error {
        UnbrkError::Timeout {
            stage,
            operation,
            recent_console,
            ..
        } => {
            assert_eq!(stage, RecoveryStage::FipPrompt);
            assert_eq!(operation, "the BL31 + U-Boot FIP prompt");
            assert!(!recent_console.is_empty());
        }
        other => panic!("expected a timeout from the injected replay failure, got {other:?}"),
    }
}

#[test]
fn missing_initial_prompt_times_out_with_recovery_context() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_overrides([(
            ReplayPoint::InitialPrompt,
            timed_out_step("no stage-one prompt"),
        )])
        .unwrap_err();

    assert_timeout(
        error,
        RecoveryStage::PreloaderPrompt,
        "the initial recovery prompt",
    );
}

#[test]
fn missing_second_prompt_times_out_with_recovery_context() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_overrides([(
            ReplayPoint::FipPrompt,
            timed_out_step("no stage-two prompt"),
        )])
        .unwrap_err();

    assert_timeout(
        error,
        RecoveryStage::FipPrompt,
        "the BL31 + U-Boot FIP prompt",
    );
}

#[test]
fn xmodem_failure_without_forward_progress_surfaces_a_typed_transfer_failure() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path()
        .unwrap()
        .with_xmodem(XmodemConfig::new(Duration::ZERO, 10, 1));
    let error = scenario
        .run_with_script_overrides([
            (
                ReplayPoint::PreloaderEotResponse,
                vec![MockStep::Read(vec![XMODEM_NAK])],
            ),
            (
                ReplayPoint::FipPrompt,
                vec![timed_out_step(
                    "stage two never appeared after cancellation",
                )],
            ),
        ])
        .unwrap_err();

    match error {
        UnbrkError::Xmodem {
            stage,
            detail,
            recent_console,
        } => {
            assert_eq!(stage, TransferStage::Preloader);
            assert!(detail.contains("receiver rejected EOT"));
            assert!(detail.contains("no forward progress"));
            assert!(!recent_console.is_empty());
        }
        other => panic!("expected an XMODEM failure, got {other:?}"),
    }
}

#[test]
fn failed_final_eot_is_tolerated_when_the_next_prompt_arrives() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path()
        .unwrap()
        .with_xmodem(XmodemConfig::new(Duration::ZERO, 10, 1));
    let run = scenario
        .run_with_script_overrides([(
            ReplayPoint::PreloaderEotResponse,
            vec![MockStep::Read(vec![XMODEM_NAK])],
        )])
        .unwrap();

    assert!(run.report.preloader_recovered_from_eot_quirk);
    assert!(run.report.events.iter().any(|event| matches!(
        event.payload,
        EventPayload::XmodemCompleted {
            stage: TransferStage::Preloader,
            recovered_from_eot_quirk: true,
            ..
        }
    )));
    run.transport.assert_finished();
}

#[test]
fn noisy_console_output_between_recovery_stages_is_tolerated() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario
        .run_with_script_overrides([(
            ReplayPoint::InterstageChatter,
            vec![MockStep::Read(
                b"\r\n*** noisy boot chatter ***\r\n\x00\x01status=warming-up\r\n".to_vec(),
            )],
        )])
        .unwrap();

    let console = String::from_utf8_lossy(&run.report.console);
    assert!(console.contains("noisy boot chatter"));
    assert_eq!(
        run.report.states.as_slice(),
        FixtureRecoveryScenario::expected_states()
    );
    run.transport.assert_finished();
}

#[test]
fn long_dram_boot_chatter_between_recovery_stages_is_tolerated() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let long_chatter = b"DRAM training...\r\n".repeat(32);
    let run = scenario
        .run_with_script_overrides([(
            ReplayPoint::InterstageChatter,
            vec![
                MockStep::Read(long_chatter[..long_chatter.len() / 2].to_vec()),
                MockStep::Delay(Duration::from_millis(10)),
                MockStep::Read(long_chatter[long_chatter.len() / 2..].to_vec()),
            ],
        )])
        .unwrap();

    assert!(
        String::from_utf8_lossy(&run.report.console).contains("DRAM training"),
        "expected injected DRAM chatter in console transcript"
    );
    run.transport.assert_finished();
}

#[test]
fn ansi_heavy_boot_menu_output_before_uboot_is_tolerated() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario
        .run_with_script_overrides([(
            ReplayPoint::UbootBootNoise,
            vec![MockStep::Read(
                b"\x1b[2J\x1b[H\x1b[33mBoot Menu\x1b[0m\r\n1. Recovery\r\n2. Normal Boot\r\n"
                    .to_vec(),
            )],
        )])
        .unwrap();

    assert!(
        String::from_utf8_lossy(&run.report.console).contains("\u{1b}[33mBoot Menu"),
        "expected injected ANSI boot-menu noise in console transcript"
    );
    run.transport.assert_finished();
}

#[test]
fn crc_readiness_with_interleaved_control_bytes_is_tolerated() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario
        .run_with_script_overrides([
            (
                ReplayPoint::PreloaderCrc,
                vec![MockStep::Read(b"C\x00C\rC".to_vec())],
            ),
            (
                ReplayPoint::FipCrc,
                vec![MockStep::Read(b"C\x7fC\nC".to_vec())],
            ),
        ])
        .unwrap();

    assert!(matches!(
        &run.report.events[2].payload,
        EventPayload::CrcReady {
            stage: TransferStage::Preloader,
            readiness_bytes_seen,
        } if *readiness_bytes_seen >= 3
    ));
    assert!(matches!(
        &run.report.events[8].payload,
        EventPayload::CrcReady {
            stage: TransferStage::Fip,
            readiness_bytes_seen,
        } if *readiness_bytes_seen >= 3
    ));
    run.transport.assert_finished();
}

#[test]
fn happy_path_fixtures_drive_recovery_and_flash_end_to_end() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario.run_with_flash().unwrap();

    assert_eq!(
        run.recovery.states.as_slice(),
        FixtureRecoveryScenario::expected_states()
    );
    assert_eq!(run.flash.loadaddr, LoadAddr::new(0x8180_0000));
    assert_eq!(run.flash.reset_evidence, "EcoNet System Reset");
    assert!(!run.flash.preloader_recovered_from_eot_quirk);
    assert!(!run.flash.fip_recovered_from_eot_quirk);

    let recovery_console = String::from_utf8_lossy(&run.recovery.console);
    assert_eq!(run.recovery.console, run.expected_recovery_console);
    assert!(recovery_console.contains("U-Boot 2026.01-OpenWrt"));

    assert!(matches!(
        &run.flash.events[0].payload,
        EventPayload::UBootPromptSeen { prompt } if prompt == "AN7581>"
    ));
    assert!(run.flash.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UBootCommandCompleted { command, summary, .. }
            if command == "printenv loadaddr" && summary.as_deref() == Some("loadaddr read")
    )));
    assert!(run.flash.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UBootCommandCompleted { command, summary, .. }
            if command == "mmc erase 0x0 0x800" && summary.as_deref() == Some("erase completed")
    )));
    assert_eq!(
        run.flash
            .events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::UBootCommandCompleted { command, summary, .. }
                    if command == "loadx $loadaddr 115200"
                        && summary.as_deref() == Some("loadx completed")
            ))
            .count(),
        2
    );
    assert_eq!(
        run.flash
            .events
            .iter()
            .filter(|event| matches!(&event.payload, EventPayload::ImageVerified { .. }))
            .count(),
        2
    );
    assert!(run.flash.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ResetSeen { evidence } if evidence == "EcoNet System Reset"
    )));
    assert!(run.flash.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::HandoffReady {
            interactive_console: false,
        }
    )));

    let flash_console = String::from_utf8_lossy(&run.flash.console);
    assert!(flash_console.contains("2048 blocks erased: OK"));
    assert!(flash_console.contains("1 blocks written: OK"));
    assert!(flash_console.contains("EcoNet System Reset"));

    run.transport.assert_finished();
}

#[test]
fn mmc_erase_failure_is_reported_with_console_context() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_flash_overrides([(
            ReplayPoint::EraseOutput,
            vec![MockStep::Read(
                b"AN7581> mmc erase 0x0 0x800\r\nerase failed\r\nAN7581> ".to_vec(),
            )],
        )])
        .unwrap_err();

    match error {
        UnbrkError::Protocol {
            stage,
            detail,
            recent_console,
        } => {
            assert_eq!(stage, RecoveryStage::UBoot);
            assert!(detail.contains("MMC erase did not report success"));
            assert!(recent_console.as_lossy_str().contains("erase failed"));
        }
        other => panic!("expected a protocol error for failed erase, got {other:?}"),
    }
}

#[test]
fn oversized_images_are_rejected_before_flash_io_begins() {
    let preloader = TempFile::with_size(129_025);
    let fip = TempFile::with_bytes(&[0x22; 4]);
    let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
    let mut transport = MockTransport::new([]);

    let error = flash_from_uboot(
        &mut transport,
        AN7581,
        &plan,
        FlashConfig::default(),
        |_| {},
    )
    .unwrap_err();

    match error {
        UnbrkError::BadInput { message } => {
            assert!(message.contains("exceeds the allocated flash window"));
        }
        other => panic!("expected bad input for oversized image, got {other:?}"),
    }
    assert!(transport.writes().is_empty());
}

#[test]
fn filesize_mismatch_after_loadx_is_reported() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_flash_overrides([(
            ReplayPoint::FilesizePreloaderOutput,
            vec![MockStep::Read(
                b"AN7581> printenv filesize\r\nfilesize=3\r\nAN7581> ".to_vec(),
            )],
        )])
        .unwrap_err();

    match error {
        UnbrkError::VerificationMismatch {
            image,
            expected_bytes,
            observed_bytes,
            recent_console,
        } => {
            assert_eq!(image, unbrk_core::ImageKind::Preloader);
            assert_eq!(expected_bytes, 4);
            assert_eq!(observed_bytes, 3);
            assert!(recent_console.as_lossy_str().contains("filesize=3"));
        }
        other => panic!("expected a verification mismatch, got {other:?}"),
    }
}

#[test]
fn mmc_write_failure_is_reported_with_console_context() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_flash_overrides([(
            ReplayPoint::MmcWritePreloaderOutput,
            vec![MockStep::Read(
                b"AN7581> mmc write $loadaddr 0x4 0x1\r\nwrite failed\r\nAN7581> ".to_vec(),
            )],
        )])
        .unwrap_err();

    match error {
        UnbrkError::Protocol {
            stage,
            detail,
            recent_console,
        } => {
            assert_eq!(stage, RecoveryStage::UBoot);
            assert!(detail.contains("MMC write did not report success"));
            assert!(recent_console.as_lossy_str().contains("write failed"));
        }
        other => panic!("expected a protocol error for failed write, got {other:?}"),
    }
}

#[test]
fn missing_reset_evidence_after_flash_times_out() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let error = scenario
        .run_with_flash_overrides([(
            ReplayPoint::ResetOutput,
            vec![MockStep::Read(
                b"resetting...\r\nstill waiting...\r\n".to_vec(),
            )],
        )])
        .unwrap_err();

    assert_timeout(error, RecoveryStage::FlashPlan, "post-flash reset output");
}

#[test]
fn fragmented_prompts_across_reads_are_accepted() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario
        .run_with_script_overrides([
            (
                ReplayPoint::InitialPrompt,
                vec![
                    MockStep::Read(b"Pr".to_vec()),
                    MockStep::Read(b"ess ".to_vec()),
                    MockStep::Read(b"x\r\n".to_vec()),
                ],
            ),
            (
                ReplayPoint::FipPrompt,
                vec![
                    MockStep::Read(b"Press x to load BL31 + ".to_vec()),
                    MockStep::Delay(Duration::from_millis(10)),
                    MockStep::Read(b"U-Boot FIP\r\n".to_vec()),
                ],
            ),
            (
                ReplayPoint::UbootPrompt,
                vec![
                    MockStep::Read(b"AN".to_vec()),
                    MockStep::Read(b"7581".to_vec()),
                    MockStep::Read(b"> \r\n".to_vec()),
                ],
            ),
        ])
        .unwrap();

    assert_eq!(
        run.report.states.as_slice(),
        FixtureRecoveryScenario::expected_states()
    );
    run.transport.assert_finished();
}

#[test]
fn harness_does_not_depend_on_ack_bytes_leaking_from_crc_fixtures() {
    let scenario = FixtureRecoveryScenario::an7581_happy_path().unwrap();
    let run = scenario
        .run_with_overrides([
            (ReplayPoint::PreloaderCrc, MockStep::Read(b"CCC".to_vec())),
            (ReplayPoint::FipCrc, MockStep::Read(b"CCC".to_vec())),
        ])
        .unwrap();

    assert_eq!(
        run.report.states.as_slice(),
        FixtureRecoveryScenario::expected_states()
    );
    run.transport.assert_finished();
}

fn timed_out_step(message: &str) -> MockStep {
    MockStep::ReadError {
        kind: io::ErrorKind::TimedOut,
        message: message.to_owned(),
    }
}

fn assert_timeout(error: UnbrkError, stage: RecoveryStage, operation: &'static str) {
    match error {
        UnbrkError::Timeout {
            stage: actual_stage,
            operation: actual_operation,
            recent_console,
            ..
        } => {
            assert_eq!(actual_stage, stage);
            assert_eq!(actual_operation, operation);
            assert!(!recent_console.is_empty() || matches!(stage, RecoveryStage::PreloaderPrompt));
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn with_bytes(bytes: &[u8]) -> Self {
        let path = unique_temp_path();
        fs::write(&path, bytes).expect("temp fixture image must be writable");
        Self { path }
    }

    fn with_size(size: usize) -> Self {
        Self::with_bytes(&vec![0x11; size])
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ignored = fs::remove_file(&self.path);
    }
}

fn unique_temp_path() -> PathBuf {
    let unique_id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "unbrk-integration-test-{}-{unique_id}.bin",
        std::process::id()
    ))
}
