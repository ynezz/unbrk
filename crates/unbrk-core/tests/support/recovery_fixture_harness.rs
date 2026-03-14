use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use unbrk_core::error::UnbrkError;
use unbrk_core::target::AN7581;
use unbrk_core::xmodem::{XMODEM_EOT, XmodemConfig, build_crc_packet};
use unbrk_core::{
    MockStep, MockTransport, RecoveryConfig, RecoveryImages, RecoveryReport, RecoveryState,
    recover_to_uboot,
};

const FIXTURE_ROOT: &str = "../../tests/fixtures/an7581";
const PROMPT_TIMEOUT: Duration = Duration::from_secs(1);
const PRELOADER_BYTES: [u8; 4] = [0x11; 4];
const FIP_BYTES: [u8; 4] = [0x22; 4];

const EXPECTED_STATES: [RecoveryState; 10] = [
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
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPoint {
    InitialPrompt,
    PreloaderCrc,
    InterstageChatter,
    FipPrompt,
    FipCrc,
    UbootBootNoise,
    UbootPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LabeledStep {
    point: ReplayPoint,
    step: MockStep,
}

impl LabeledStep {
    const fn new(point: ReplayPoint, step: MockStep) -> Self {
        Self { point, step }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryFixtures {
    initial_prompt: Vec<u8>,
    preloader_crc: Vec<u8>,
    interstage_chatter: Vec<u8>,
    fip_prompt: Vec<u8>,
    fip_crc: Vec<u8>,
    uboot_boot_noise: Vec<u8>,
    uboot_prompts: Vec<u8>,
}

impl RecoveryFixtures {
    fn load() -> io::Result<Self> {
        Ok(Self {
            initial_prompt: Self::read("happy-path-stage1-prompt.bin")?,
            preloader_crc: Self::read("happy-path-stage1-crc-readiness.bin")?,
            interstage_chatter: Self::read("happy-path-interstage-chatter.bin")?,
            fip_prompt: Self::read("happy-path-stage2-prompt.bin")?,
            fip_crc: Self::read("happy-path-stage2-crc-readiness.bin")?,
            uboot_boot_noise: Self::read("happy-path-uboot-boot-noise.bin")?,
            uboot_prompts: Self::read("happy-path-uboot-prompts.bin")?,
        })
    }

    fn expected_recovery_console(&self) -> Vec<u8> {
        let mut console = Vec::with_capacity(
            self.initial_prompt.len()
                + self.preloader_crc.len()
                + self.interstage_chatter.len()
                + self.fip_prompt.len()
                + self.fip_crc.len()
                + self.uboot_boot_noise.len()
                + self.uboot_prompts.len(),
        );
        console.extend_from_slice(&self.initial_prompt);
        console.extend_from_slice(&self.preloader_crc);
        console.extend_from_slice(&self.interstage_chatter);
        console.extend_from_slice(&self.fip_prompt);
        console.extend_from_slice(&self.fip_crc);
        console.extend_from_slice(&self.uboot_boot_noise);
        console.extend_from_slice(&self.uboot_prompts);
        console
    }

    fn read(file_name: &str) -> io::Result<Vec<u8>> {
        fs::read(Self::root().join(file_name))
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ROOT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureRecoveryScenario {
    fixtures: RecoveryFixtures,
    prompt_timeout: Duration,
    xmodem: XmodemConfig,
    preloader_name: &'static str,
    preloader: Vec<u8>,
    fip_name: &'static str,
    fip: Vec<u8>,
}

impl FixtureRecoveryScenario {
    /// Loads the documented AN7581 happy-path fixtures and test payloads.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if any fixture file cannot be read.
    pub fn an7581_happy_path() -> io::Result<Self> {
        Ok(Self {
            fixtures: RecoveryFixtures::load()?,
            prompt_timeout: PROMPT_TIMEOUT,
            xmodem: XmodemConfig::default(),
            preloader_name: "preloader.bin",
            preloader: PRELOADER_BYTES.to_vec(),
            fip_name: "fip.bin",
            fip: FIP_BYTES.to_vec(),
        })
    }

    #[must_use]
    pub const fn expected_states() -> &'static [RecoveryState] {
        &EXPECTED_STATES
    }

    /// Runs the happy-path fixture replay without script overrides.
    ///
    /// # Errors
    ///
    /// Returns any typed recovery failure produced by `recover_to_uboot()`.
    pub fn run(&self) -> Result<FixtureRun, UnbrkError> {
        self.run_with_overrides(std::iter::empty())
    }

    /// Runs the fixture replay with targeted scripted-step overrides.
    ///
    /// # Errors
    ///
    /// Returns any typed recovery failure produced by `recover_to_uboot()`.
    pub fn run_with_overrides<I>(&self, overrides: I) -> Result<FixtureRun, UnbrkError>
    where
        I: IntoIterator<Item = (ReplayPoint, MockStep)>,
    {
        let mut script = self.script();
        for (point, step) in overrides {
            let target = script
                .iter_mut()
                .find(|entry| {
                    entry.point == point
                        && matches!(entry.step, MockStep::Read(_) | MockStep::ReadError { .. })
                })
                .unwrap_or_else(|| panic!("missing replay point override target: {point:?}"));
            target.step = step;
        }

        let mut transport = MockTransport::new(script.into_iter().map(|entry| entry.step));
        let report = recover_to_uboot(
            &mut transport,
            AN7581,
            RecoveryImages {
                preloader_name: self.preloader_name,
                preloader: &self.preloader,
                fip_name: self.fip_name,
                fip: &self.fip,
            },
            RecoveryConfig::new(self.prompt_timeout, self.xmodem),
        )?;

        Ok(FixtureRun {
            report,
            transport,
            expected_console: self.fixtures.expected_recovery_console(),
        })
    }

    fn script(&self) -> Vec<LabeledStep> {
        let preloader_packet = build_crc_packet(1, &self.preloader);
        let fip_packet = build_crc_packet(1, &self.fip);

        vec![
            LabeledStep::new(
                ReplayPoint::InitialPrompt,
                MockStep::SetTimeout(self.prompt_timeout),
            ),
            LabeledStep::new(
                ReplayPoint::InitialPrompt,
                MockStep::Read(self.fixtures.initial_prompt.clone()),
            ),
            LabeledStep::new(ReplayPoint::InitialPrompt, MockStep::Write(vec![b'x'])),
            LabeledStep::new(ReplayPoint::InitialPrompt, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::PreloaderCrc,
                MockStep::SetTimeout(self.prompt_timeout),
            ),
            LabeledStep::new(
                ReplayPoint::PreloaderCrc,
                MockStep::Read(self.fixtures.preloader_crc.clone()),
            ),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Write(preloader_packet)),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Flush),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Write(vec![XMODEM_EOT])),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::InterstageChatter,
                MockStep::SetTimeout(self.prompt_timeout),
            ),
            LabeledStep::new(
                ReplayPoint::InterstageChatter,
                MockStep::Read(self.fixtures.interstage_chatter.clone()),
            ),
            LabeledStep::new(
                ReplayPoint::FipPrompt,
                MockStep::Read(self.fixtures.fip_prompt.clone()),
            ),
            LabeledStep::new(ReplayPoint::FipPrompt, MockStep::Write(vec![b'x'])),
            LabeledStep::new(ReplayPoint::FipPrompt, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::FipCrc,
                MockStep::SetTimeout(self.prompt_timeout),
            ),
            LabeledStep::new(
                ReplayPoint::FipCrc,
                MockStep::Read(self.fixtures.fip_crc.clone()),
            ),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Write(fip_packet)),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Flush),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Write(vec![XMODEM_EOT])),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::UbootBootNoise,
                MockStep::SetTimeout(self.prompt_timeout),
            ),
            LabeledStep::new(
                ReplayPoint::UbootBootNoise,
                MockStep::Read(self.fixtures.uboot_boot_noise.clone()),
            ),
            LabeledStep::new(
                ReplayPoint::UbootPrompt,
                MockStep::Read(self.fixtures.uboot_prompts.clone()),
            ),
        ]
    }
}

#[derive(Debug)]
pub struct FixtureRun {
    pub report: RecoveryReport,
    pub transport: MockTransport,
    pub expected_console: Vec<u8>,
}
