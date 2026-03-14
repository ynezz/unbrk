//! Target profile types and built-in board definitions.

use crate::error::UnbrkError;
use crate::event::{ImageKind, RecoveryStage, TransferStage};
use regex::{Error as RegexError, bytes::Regex};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

const AN7581_RECOVERY_STAGE_ORDER: [RecoveryStage; 5] = [
    RecoveryStage::Bootrom,
    RecoveryStage::PreloaderPrompt,
    RecoveryStage::FipPrompt,
    RecoveryStage::UBoot,
    RecoveryStage::FlashPlan,
];

const AN7581_RECOVERY_TRANSFER_ORDER: [TransferStage; 2] =
    [TransferStage::Preloader, TransferStage::Fip];

const AN7581_FLASH_TRANSFER_ORDER: [TransferStage; 2] =
    [TransferStage::LoadxPreloader, TransferStage::LoadxFip];

/// Built-in profile for the Airoha AN7581 recovery flow.
#[allow(clippy::module_name_repetitions)]
pub const AN7581: TargetProfile = TargetProfile {
    name: "an7581",
    serial: SerialSettings {
        baud_rate: 115_200,
        data_bits: DataBits::Eight,
        parity: Parity::None,
        stop_bits: StopBits::One,
        flow_control: FlowControl::None,
    },
    prompts: PromptPatterns {
        initial_recovery: PromptPattern::new(r"Press x"),
        second_stage: PromptPattern::new(r"Press x to load BL31 \+ U-Boot FIP"),
        uboot: PromptPattern::new(r"AN7581>"),
    },
    flash: FlashLayout {
        block_size: MmcBlockSize::new(512),
        erase_range: BlockRange::new(BlockOffset::new(0), BlockCount::new(0x800)),
        preloader: BlockRange::new(BlockOffset::new(0x4), BlockCount::new(0xfc)),
        fip: BlockRange::new(BlockOffset::new(0x100), BlockCount::new(0x700)),
    },
    recovery_stage_order: &AN7581_RECOVERY_STAGE_ORDER,
    recovery_transfer_order: &AN7581_RECOVERY_TRANSFER_ORDER,
    flash_transfer_order: &AN7581_FLASH_TRANSFER_ORDER,
};

/// Strongly typed target definition for a single supported board profile.
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetProfile {
    pub name: &'static str,
    pub serial: SerialSettings,
    pub prompts: PromptPatterns,
    pub flash: FlashLayout,
    pub recovery_stage_order: &'static [RecoveryStage],
    pub recovery_transfer_order: &'static [TransferStage],
    pub flash_transfer_order: &'static [TransferStage],
}

impl TargetProfile {
    /// Validates the regex-backed prompt definitions.
    ///
    /// # Errors
    ///
    /// Returns the first regex compilation failure.
    pub fn validate(&self) -> Result<(), RegexError> {
        self.prompts.validate()
    }

    /// Builds the default flash plan for this target profile.
    #[must_use]
    pub fn flash_plan(&self, preloader: impl Into<PathBuf>, fip: impl Into<PathBuf>) -> FlashPlan {
        FlashPlan {
            block_size: self.flash.block_size,
            erase_ranges: vec![EraseRange::new(
                self.flash.erase_range.start_block,
                self.flash.erase_range.block_count,
            )],
            write_stages: vec![
                WriteStage::new(
                    ImageKind::Preloader,
                    self.flash.preloader.start_block,
                    self.flash.preloader.block_count,
                    preloader,
                ),
                WriteStage::new(
                    ImageKind::Fip,
                    self.flash.fip.start_block,
                    self.flash.fip.block_count,
                    fip,
                ),
            ],
        }
    }
}

/// Serial defaults for a concrete target profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialSettings {
    pub baud_rate: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

/// Prompt matchers for the recovery and U-Boot phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPatterns {
    pub initial_recovery: PromptPattern,
    pub second_stage: PromptPattern,
    pub uboot: PromptPattern,
}

impl PromptPatterns {
    /// Validates that all prompt regexes compile.
    ///
    /// # Errors
    ///
    /// Returns the first regex compilation failure.
    pub fn validate(&self) -> Result<(), RegexError> {
        self.initial_recovery.compile()?;
        self.second_stage.compile()?;
        self.uboot.compile()?;
        Ok(())
    }
}

/// Raw regex source for a board-specific prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPattern {
    source: Cow<'static, str>,
}

impl PromptPattern {
    /// Creates a prompt pattern from a static regex source string.
    #[must_use]
    pub const fn new(source: &'static str) -> Self {
        Self {
            source: Cow::Borrowed(source),
        }
    }

    /// Creates a prompt pattern from an owned runtime regex source string.
    #[must_use]
    pub fn from_owned(source: impl Into<String>) -> Self {
        Self {
            source: Cow::Owned(source.into()),
        }
    }

    /// Returns the raw regex source string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.source.as_ref()
    }

    /// Compiles the prompt regex.
    ///
    /// # Errors
    ///
    /// Returns the `regex` crate's bytes-regex compilation error.
    pub fn compile(&self) -> Result<Regex, RegexError> {
        Regex::new(self.source.as_ref())
    }
}

/// Flash layout values for a concrete target profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashLayout {
    pub block_size: MmcBlockSize,
    pub erase_range: BlockRange,
    pub preloader: BlockRange,
    pub fip: BlockRange,
}

impl FlashLayout {
    /// Returns the write range for a given image kind.
    #[must_use]
    pub const fn range_for(self, image: ImageKind) -> BlockRange {
        match image {
            ImageKind::Preloader => self.preloader,
            ImageKind::Fip => self.fip,
        }
    }
}

/// Flash erase range expressed in MMC blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EraseRange {
    pub start_block: BlockOffset,
    pub block_count: BlockCount,
}

impl EraseRange {
    /// Creates an erase range from a start offset and block count.
    #[must_use]
    pub const fn new(start_block: BlockOffset, block_count: BlockCount) -> Self {
        Self {
            start_block,
            block_count,
        }
    }

    /// Returns the erase range as a generic block span.
    #[must_use]
    pub const fn as_block_range(self) -> BlockRange {
        BlockRange::new(self.start_block, self.block_count)
    }
}

/// One flash write step tied to a concrete image path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStage {
    pub image: ImageKind,
    pub start_block: BlockOffset,
    pub block_count: BlockCount,
    pub image_path: PathBuf,
}

impl WriteStage {
    /// Creates a write stage from the target block range and image path.
    #[must_use]
    pub fn new(
        image: ImageKind,
        start_block: BlockOffset,
        block_count: BlockCount,
        image_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            image,
            start_block,
            block_count,
            image_path: image_path.into(),
        }
    }

    /// Returns the write stage as a generic block span.
    #[must_use]
    pub const fn as_block_range(&self) -> BlockRange {
        BlockRange::new(self.start_block, self.block_count)
    }

    /// Returns the maximum image size that fits in this stage.
    #[must_use]
    pub fn max_bytes(&self, block_size: MmcBlockSize) -> u64 {
        self.as_block_range().byte_len(block_size)
    }

    /// Validates an image size against this stage's allocated block range.
    ///
    /// # Errors
    ///
    /// Returns `UnbrkError::BadInput` when the image is empty or exceeds the
    /// allocated flash window.
    pub fn validate_image_size(
        &self,
        block_size: MmcBlockSize,
        image_size: u64,
    ) -> Result<(), UnbrkError> {
        if image_size == 0 {
            return Err(UnbrkError::BadInput {
                message: format!(
                    "{image} image at {} is empty",
                    self.image_path.display(),
                    image = self.image,
                ),
            });
        }

        let max_bytes = self.max_bytes(block_size);
        if image_size > max_bytes {
            return Err(UnbrkError::BadInput {
                message: format!(
                    "{image} image at {} is {image_size} bytes, which exceeds the allocated flash window of {max_bytes} bytes",
                    self.image_path.display(),
                    image = self.image,
                ),
            });
        }

        Ok(())
    }

    /// Validates the on-disk image for this write stage.
    ///
    /// # Errors
    ///
    /// Returns `UnbrkError::BadInput` when the file cannot be read, is empty,
    /// or exceeds the allocated flash window.
    pub fn validate_image_path(&self, block_size: MmcBlockSize) -> Result<(), UnbrkError> {
        let image_size =
            file_size_bytes(self.image_path.as_path()).map_err(|error| UnbrkError::BadInput {
                message: format!(
                    "failed to inspect {image} image at {}: {error}",
                    self.image_path.display(),
                    image = self.image,
                ),
            })?;

        self.validate_image_size(block_size, image_size)
    }
}

/// Full destructive flash plan built from typed erase and write stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashPlan {
    pub block_size: MmcBlockSize,
    pub erase_ranges: Vec<EraseRange>,
    pub write_stages: Vec<WriteStage>,
}

impl FlashPlan {
    /// Validates all staged images before any erase or write is attempted.
    ///
    /// # Errors
    ///
    /// Returns the first `UnbrkError::BadInput` encountered.
    pub fn validate_image_sizes(&self) -> Result<(), UnbrkError> {
        for stage in &self.write_stages {
            stage.validate_image_path(self.block_size)?;
        }

        Ok(())
    }
}

/// Inclusive-start block range stored as offset + count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    pub start_block: BlockOffset,
    pub block_count: BlockCount,
}

impl BlockRange {
    /// Creates a block range from a start offset and block count.
    ///
    /// # Panics
    ///
    /// Panics when `start_block + block_count` would overflow `u32`.
    #[must_use]
    pub const fn new(start_block: BlockOffset, block_count: BlockCount) -> Self {
        assert!(
            start_block.get().checked_add(block_count.get()).is_some(),
            "block range end must fit in u32",
        );

        Self {
            start_block,
            block_count,
        }
    }

    /// Returns the exclusive end block.
    ///
    /// # Panics
    ///
    /// Panics when the stored range would overflow `u32`.
    #[must_use]
    pub const fn end_block(self) -> BlockOffset {
        match self.start_block.get().checked_add(self.block_count.get()) {
            Some(end_block) => BlockOffset::new(end_block),
            None => panic!("block range end must fit in u32"),
        }
    }

    /// Returns the maximum payload size in bytes for this range.
    #[must_use]
    pub fn byte_len(self, block_size: MmcBlockSize) -> u64 {
        self.block_count.bytes(block_size)
    }
}

/// MMC block offset from the start of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockOffset(u32);

impl BlockOffset {
    /// Creates a block offset.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw block offset value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Number of contiguous MMC blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockCount(u32);

impl BlockCount {
    /// Creates a block count.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw block-count value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the capacity covered by this block count.
    #[must_use]
    pub fn bytes(self, block_size: MmcBlockSize) -> u64 {
        u64::from(self.0) * u64::from(block_size.get())
    }
}

/// Size of one MMC block in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MmcBlockSize(u32);

impl MmcBlockSize {
    /// Creates an MMC block size.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw MMC block size in bytes.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

fn file_size_bytes(path: &Path) -> std::io::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::{
        AN7581, AN7581_FLASH_TRANSFER_ORDER, AN7581_RECOVERY_STAGE_ORDER,
        AN7581_RECOVERY_TRANSFER_ORDER, BlockCount, BlockOffset, BlockRange, FlashPlan,
        MmcBlockSize,
    };
    use crate::error::UnbrkError;
    use crate::event::{ImageKind, RecoveryStage, TransferStage};
    use serialport::{DataBits, FlowControl, Parity, StopBits};
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn an7581_profile_regexes_compile() {
        assert!(AN7581.validate().is_ok());
    }

    #[test]
    fn an7581_serial_defaults_match_documented_line_settings() {
        assert_eq!(AN7581.serial.baud_rate, 115_200);
        assert_eq!(AN7581.serial.data_bits, DataBits::Eight);
        assert_eq!(AN7581.serial.parity, Parity::None);
        assert_eq!(AN7581.serial.stop_bits, StopBits::One);
        assert_eq!(AN7581.serial.flow_control, FlowControl::None);
    }

    #[test]
    fn an7581_flash_layout_matches_the_protocol_doc() {
        assert_eq!(AN7581.flash.block_size, MmcBlockSize::new(512));
        assert_eq!(AN7581.flash.erase_range.start_block, BlockOffset::new(0));
        assert_eq!(AN7581.flash.erase_range.block_count, BlockCount::new(0x800));
        assert_eq!(AN7581.flash.preloader.start_block, BlockOffset::new(0x4));
        assert_eq!(AN7581.flash.preloader.block_count, BlockCount::new(0xfc));
        assert_eq!(AN7581.flash.fip.start_block, BlockOffset::new(0x100));
        assert_eq!(AN7581.flash.fip.block_count, BlockCount::new(0x700));
    }

    #[test]
    fn an7581_stage_order_matches_expected_recovery_flow() {
        assert_eq!(
            AN7581.recovery_stage_order,
            &[
                RecoveryStage::Bootrom,
                RecoveryStage::PreloaderPrompt,
                RecoveryStage::FipPrompt,
                RecoveryStage::UBoot,
                RecoveryStage::FlashPlan,
            ]
        );
        assert_eq!(
            AN7581.recovery_transfer_order,
            &[TransferStage::Preloader, TransferStage::Fip]
        );
        assert_eq!(
            AN7581.flash_transfer_order,
            &[TransferStage::LoadxPreloader, TransferStage::LoadxFip]
        );
        assert_eq!(AN7581.recovery_stage_order, &AN7581_RECOVERY_STAGE_ORDER);
        assert_eq!(
            AN7581.recovery_transfer_order,
            &AN7581_RECOVERY_TRANSFER_ORDER
        );
        assert_eq!(AN7581.flash_transfer_order, &AN7581_FLASH_TRANSFER_ORDER);
    }

    #[test]
    fn initial_prompt_pattern_also_matches_the_second_stage_text() {
        let initial_prompt = AN7581.prompts.initial_recovery.compile().unwrap();
        let second_stage = "Press x to load BL31 + U-Boot FIP";

        assert!(initial_prompt.is_match(second_stage.as_bytes()));
    }

    #[test]
    fn flash_ranges_return_expected_capacity() {
        assert_eq!(AN7581.flash.preloader.end_block(), BlockOffset::new(0x100));
        assert_eq!(
            AN7581.flash.preloader.byte_len(AN7581.flash.block_size),
            129_024
        );
        assert_eq!(AN7581.flash.fip.byte_len(AN7581.flash.block_size), 917_504);
    }

    #[test]
    fn flash_layout_can_select_ranges_by_image_kind() {
        assert_eq!(
            AN7581.flash.range_for(ImageKind::Preloader),
            AN7581.flash.preloader
        );
        assert_eq!(AN7581.flash.range_for(ImageKind::Fip), AN7581.flash.fip);
    }

    #[test]
    fn an7581_flash_plan_uses_the_documented_ranges() {
        let plan = AN7581.flash_plan("preloader.bin", "fip.bin");

        assert_eq!(plan.block_size, MmcBlockSize::new(512));
        assert_eq!(plan.erase_ranges.len(), 1);
        assert_eq!(plan.erase_ranges[0].start_block, BlockOffset::new(0));
        assert_eq!(plan.erase_ranges[0].block_count, BlockCount::new(0x800));
        assert_eq!(plan.write_stages.len(), 2);
        assert_eq!(plan.write_stages[0].image, ImageKind::Preloader);
        assert_eq!(plan.write_stages[0].start_block, BlockOffset::new(0x4));
        assert_eq!(plan.write_stages[0].block_count, BlockCount::new(0xfc));
        assert_eq!(plan.write_stages[1].image, ImageKind::Fip);
        assert_eq!(plan.write_stages[1].start_block, BlockOffset::new(0x100));
        assert_eq!(plan.write_stages[1].block_count, BlockCount::new(0x700));
    }

    #[test]
    fn flash_plan_accepts_images_that_fit_exactly() {
        let preloader = temp_file_with_size(129_024);
        let fip = temp_file_with_size(917_504);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());

        assert!(plan.validate_image_sizes().is_ok());
    }

    #[test]
    fn block_range_new_rejects_overflowing_end_blocks() {
        let result = std::panic::catch_unwind(|| {
            BlockRange::new(BlockOffset::new(u32::MAX), BlockCount::new(1))
        });

        assert!(result.is_err());
    }

    #[test]
    fn flash_plan_accepts_smaller_images() {
        let preloader = temp_file_with_size(64_000);
        let fip = temp_file_with_size(512_000);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());

        assert!(plan.validate_image_sizes().is_ok());
    }

    #[test]
    fn flash_plan_rejects_images_that_exceed_by_one_byte() {
        let preloader = temp_file_with_size(129_025);
        let fip = temp_file_with_size(512_000);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());

        assert_bad_input_contains(&plan, "exceeds the allocated flash window");
    }

    #[test]
    fn flash_plan_rejects_images_that_exceed_by_a_full_block() {
        let preloader = temp_file_with_size(129_024 + 512);
        let fip = temp_file_with_size(512_000);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());

        assert_bad_input_contains(&plan, "exceeds the allocated flash window");
    }

    #[test]
    fn flash_plan_rejects_zero_size_images() {
        let preloader = temp_file_with_size(0);
        let fip = temp_file_with_size(512_000);
        let plan = AN7581.flash_plan(preloader.path.clone(), fip.path.clone());

        assert_bad_input_contains(&plan, "is empty");
    }

    fn assert_bad_input_contains(plan: &FlashPlan, expected_fragment: &str) {
        match plan.validate_image_sizes().unwrap_err() {
            UnbrkError::BadInput { message } => {
                assert!(message.contains(expected_fragment));
            }
            other => panic!("expected bad input error, got {other}"),
        }
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
            "unbrk-target-tests-{}-{unique_id}.bin",
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
}
