use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use unbrk_core::error::UnbrkError;
use unbrk_core::target::AN7581;
use unbrk_core::xmodem::{
    XMODEM_ACK, XMODEM_CRC_READY_MIN_BYTES, XMODEM_EOT, XmodemConfig, build_crc_packet,
};
use unbrk_core::{
    FlashConfig, FlashReport, MockStep, MockTransport, RecoveryConfig, RecoveryImages,
    RecoveryReport, RecoveryState, flash_from_uboot, recover_to_uboot,
};

const FIXTURE_ROOT: &str = "../../tests/fixtures/an7581";
const PROMPT_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const RESET_TIMEOUT: Duration = Duration::from_secs(1);
const PRELOADER_BYTES: [u8; 4] = [0x11; 4];
const FIP_BYTES: [u8; 4] = [0x22; 4];
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    PreloaderTransferAck,
    PreloaderEotResponse,
    InterstageChatter,
    FipPrompt,
    FipCrc,
    FipTransferAck,
    FipEotResponse,
    UbootBootNoise,
    UbootPrompt,
    FlashWakePrompt,
    LoadaddrOutput,
    EraseOutput,
    LoadxPreloaderCrc,
    LoadxPreloaderTransferAck,
    LoadxPreloaderEotResponse,
    LoadxPreloaderOutput,
    FilesizePreloaderOutput,
    MmcWritePreloaderOutput,
    LoadxFipCrc,
    LoadxFipTransferAck,
    LoadxFipEotResponse,
    LoadxFipOutput,
    FilesizeFipOutput,
    MmcWriteFipOutput,
    ResetOutput,
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
            preloader_crc: Self::trim_crc_ready(&Self::read(
                "happy-path-stage1-crc-readiness.bin",
            )?)?,
            interstage_chatter: Self::read("happy-path-interstage-chatter.bin")?,
            fip_prompt: Self::read("happy-path-stage2-prompt.bin")?,
            fip_crc: Self::trim_crc_ready(&Self::read("happy-path-stage2-crc-readiness.bin")?)?,
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

    fn trim_crc_ready(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let ready_len = bytes.iter().take_while(|&&byte| byte == b'C').count();
        if ready_len < usize::try_from(XMODEM_CRC_READY_MIN_BYTES).unwrap_or(usize::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CRC readiness fixture must start with a leading C burst",
            ));
        }

        Ok(bytes[..ready_len].to_vec())
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
    pub const fn with_xmodem(mut self, xmodem: XmodemConfig) -> Self {
        self.xmodem = xmodem;
        self
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

    /// Runs the fixture replay through recovery and the persistent flash phase.
    ///
    /// # Errors
    ///
    /// Returns any typed recovery or flash failure from the core flows.
    pub fn run_with_flash(&self) -> Result<FixtureRecoveryAndFlashRun, UnbrkError> {
        self.run_with_flash_overrides(std::iter::empty::<(ReplayPoint, [MockStep; 0])>())
    }

    /// Runs the fixture replay through recovery and the persistent flash phase
    /// with targeted scripted-step overrides.
    ///
    /// # Errors
    ///
    /// Returns any typed recovery or flash failure from the core flows.
    pub fn run_with_flash_overrides<I, J>(
        &self,
        overrides: I,
    ) -> Result<FixtureRecoveryAndFlashRun, UnbrkError>
    where
        I: IntoIterator<Item = (ReplayPoint, J)>,
        J: IntoIterator<Item = MockStep>,
    {
        let preloader = TempFile::with_bytes(&self.preloader);
        let fip = TempFile::with_bytes(&self.fip);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());
        let mut script = self.script();
        script.extend(self.flash_script());
        Self::apply_overrides(&mut script, overrides);

        let mut transport = MockTransport::new(script.into_iter().map(|entry| entry.step));
        let recovery = recover_to_uboot(
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
        let flash = flash_from_uboot(
            &mut transport,
            AN7581,
            &plan,
            FlashConfig::new(COMMAND_TIMEOUT, RESET_TIMEOUT, self.xmodem),
        )?;

        Ok(FixtureRecoveryAndFlashRun {
            recovery,
            flash,
            transport,
            expected_recovery_console: self.fixtures.expected_recovery_console(),
        })
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
        self.run_with_script_overrides(overrides.into_iter().map(|(point, step)| (point, [step])))
    }

    /// Runs the fixture replay with targeted scripted-step overrides that may
    /// replace one replay point with multiple read-oriented steps.
    ///
    /// # Errors
    ///
    /// Returns any typed recovery failure produced by `recover_to_uboot()`.
    pub fn run_with_script_overrides<I, J>(&self, overrides: I) -> Result<FixtureRun, UnbrkError>
    where
        I: IntoIterator<Item = (ReplayPoint, J)>,
        J: IntoIterator<Item = MockStep>,
    {
        let mut script = self.script();
        Self::apply_overrides(&mut script, overrides);

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

    fn apply_overrides<I, J>(script: &mut Vec<LabeledStep>, overrides: I)
    where
        I: IntoIterator<Item = (ReplayPoint, J)>,
        J: IntoIterator<Item = MockStep>,
    {
        for (point, steps) in overrides {
            let index = script
                .iter()
                .position(|entry| {
                    entry.point == point
                        && matches!(entry.step, MockStep::Read(_) | MockStep::ReadError { .. })
                })
                .unwrap_or_else(|| panic!("missing replay point override target: {point:?}"));

            let replacement = steps
                .into_iter()
                .map(|step| {
                    assert!(
                        matches!(
                            step,
                            MockStep::Read(_) | MockStep::ReadError { .. } | MockStep::Delay(_)
                        ),
                        "invalid override step for {point:?}: {step:?}",
                    );
                    LabeledStep::new(point, step)
                })
                .collect::<Vec<_>>();

            script.splice(index..=index, replacement);
        }
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
            LabeledStep::new(
                ReplayPoint::PreloaderTransferAck,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Write(vec![XMODEM_EOT])),
            LabeledStep::new(ReplayPoint::PreloaderCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::PreloaderEotResponse,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
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
            LabeledStep::new(
                ReplayPoint::FipTransferAck,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Write(vec![XMODEM_EOT])),
            LabeledStep::new(ReplayPoint::FipCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::FipEotResponse,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
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

    #[allow(clippy::too_many_lines)]
    fn flash_script(&self) -> Vec<LabeledStep> {
        let mut script = Self::flash_preamble();
        script.extend(self.flash_preloader_transfer());
        script.extend(self.flash_fip_transfer());
        script.extend(Self::flash_reset_sequence());
        script
    }

    fn flash_preamble() -> Vec<LabeledStep> {
        vec![
            LabeledStep::new(
                ReplayPoint::FlashWakePrompt,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(ReplayPoint::FlashWakePrompt, MockStep::Write(vec![b'\r'])),
            LabeledStep::new(ReplayPoint::FlashWakePrompt, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::FlashWakePrompt,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::FlashWakePrompt,
                MockStep::Read(b"\r\nAN7581> ".to_vec()),
            ),
            LabeledStep::new(
                ReplayPoint::LoadaddrOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadaddrOutput,
                MockStep::Write(b"printenv loadaddr\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::LoadaddrOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadaddrOutput,
                MockStep::Read(
                    b"AN7581> printenv loadaddr\r\nloadaddr=0x81800000\r\nAN7581> ".to_vec(),
                ),
            ),
            LabeledStep::new(
                ReplayPoint::EraseOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::EraseOutput,
                MockStep::Write(b"mmc erase 0x0 0x800\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::EraseOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::EraseOutput,
                MockStep::Read(
                    b"AN7581> mmc erase 0x0 0x800\r\n2048 blocks erased: OK\r\nAN7581> ".to_vec(),
                ),
            ),
        ]
    }

    fn flash_preloader_transfer(&self) -> Vec<LabeledStep> {
        let preloader_packet = build_crc_packet(1, &self.preloader);

        vec![
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::Write(b"loadx $loadaddr 115200\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::LoadxPreloaderCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::Read(b"loadx $loadaddr 115200\r\nCCC".to_vec()),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::Write(preloader_packet),
            ),
            LabeledStep::new(ReplayPoint::LoadxPreloaderCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderTransferAck,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderCrc,
                MockStep::Write(vec![XMODEM_EOT]),
            ),
            LabeledStep::new(ReplayPoint::LoadxPreloaderCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderEotResponse,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxPreloaderOutput,
                MockStep::Read(b"\r\nTotal Size = 0x4 = 4 Bytes\r\nAN7581> ".to_vec()),
            ),
            LabeledStep::new(
                ReplayPoint::FilesizePreloaderOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::FilesizePreloaderOutput,
                MockStep::Write(b"printenv filesize\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::FilesizePreloaderOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::FilesizePreloaderOutput,
                MockStep::Read(b"AN7581> printenv filesize\r\nfilesize=4\r\nAN7581> ".to_vec()),
            ),
            LabeledStep::new(
                ReplayPoint::MmcWritePreloaderOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::MmcWritePreloaderOutput,
                MockStep::Write(b"mmc write $loadaddr 0x4 0xfc\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::MmcWritePreloaderOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::MmcWritePreloaderOutput,
                MockStep::Read(
                    b"AN7581> mmc write $loadaddr 0x4 0xfc\r\n252 blocks written: OK\r\nAN7581> "
                        .to_vec(),
                ),
            ),
        ]
    }

    fn flash_fip_transfer(&self) -> Vec<LabeledStep> {
        let fip_packet = build_crc_packet(1, &self.fip);

        vec![
            LabeledStep::new(
                ReplayPoint::LoadxFipCrc,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxFipCrc,
                MockStep::Write(b"loadx $loadaddr 115200\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::LoadxFipCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxFipCrc,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxFipCrc,
                MockStep::Read(b"loadx $loadaddr 115200\r\nCCC".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::LoadxFipCrc, MockStep::Write(fip_packet)),
            LabeledStep::new(ReplayPoint::LoadxFipCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxFipTransferAck,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(ReplayPoint::LoadxFipCrc, MockStep::Write(vec![XMODEM_EOT])),
            LabeledStep::new(ReplayPoint::LoadxFipCrc, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::LoadxFipEotResponse,
                MockStep::Read(vec![XMODEM_ACK]),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxFipOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::LoadxFipOutput,
                MockStep::Read(b"\r\nTotal Size = 0x4 = 4 Bytes\r\nAN7581> ".to_vec()),
            ),
            LabeledStep::new(
                ReplayPoint::FilesizeFipOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::FilesizeFipOutput,
                MockStep::Write(b"printenv filesize\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::FilesizeFipOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::FilesizeFipOutput,
                MockStep::Read(
                    b"AN7581> printenv filesize\r\nfilesize=0x4\r\nAN7581> ".to_vec(),
                ),
            ),
            LabeledStep::new(
                ReplayPoint::MmcWriteFipOutput,
                MockStep::SetTimeout(COMMAND_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::MmcWriteFipOutput,
                MockStep::Write(b"mmc write $loadaddr 0x100 0x700\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::MmcWriteFipOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::MmcWriteFipOutput,
                MockStep::Read(
                    b"AN7581> mmc write $loadaddr 0x100 0x700\r\n1792 blocks written: OK\r\nAN7581> "
                        .to_vec(),
                ),
            ),
        ]
    }

    fn flash_reset_sequence() -> Vec<LabeledStep> {
        vec![
            LabeledStep::new(
                ReplayPoint::ResetOutput,
                MockStep::SetTimeout(RESET_TIMEOUT),
            ),
            LabeledStep::new(
                ReplayPoint::ResetOutput,
                MockStep::Write(b"reset\n".to_vec()),
            ),
            LabeledStep::new(ReplayPoint::ResetOutput, MockStep::Flush),
            LabeledStep::new(
                ReplayPoint::ResetOutput,
                MockStep::Read(
                    fs::read(
                        Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("../../tests/fixtures/an7581/reset-evidence.bin"),
                    )
                    .expect("reset fixture must load"),
                ),
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

#[derive(Debug)]
pub struct FixtureRecoveryAndFlashRun {
    pub recovery: RecoveryReport,
    pub flash: FlashReport,
    pub transport: MockTransport,
    pub expected_recovery_console: Vec<u8>,
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
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ignored = fs::remove_file(&self.path);
    }
}

fn unique_temp_path() -> PathBuf {
    let unique_id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "unbrk-fixture-harness-{}-{unique_id}.bin",
        std::process::id()
    ))
}
