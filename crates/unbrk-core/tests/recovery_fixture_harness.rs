mod support;

use std::io;
use support::recovery_fixture_harness::{FixtureRecoveryScenario, ReplayPoint};
use unbrk_core::error::UnbrkError;
use unbrk_core::{EventKind, EventPayload, MockStep, RecoveryStage, TransferStage};

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
