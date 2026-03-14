//! Target profile types and built-in board definitions.

use crate::event::{ImageKind, RecoveryStage, TransferStage};
use regex::{Error as RegexError, Regex};
use serialport::{DataBits, FlowControl, Parity, StopBits};

const VALYRIAN_RECOVERY_STAGE_ORDER: [RecoveryStage; 5] = [
    RecoveryStage::Bootrom,
    RecoveryStage::PreloaderPrompt,
    RecoveryStage::FipPrompt,
    RecoveryStage::UBoot,
    RecoveryStage::FlashPlan,
];

const VALYRIAN_RECOVERY_TRANSFER_ORDER: [TransferStage; 2] =
    [TransferStage::Preloader, TransferStage::Fip];

const VALYRIAN_FLASH_TRANSFER_ORDER: [TransferStage; 2] =
    [TransferStage::LoadxPreloader, TransferStage::LoadxFip];

/// Built-in profile for the Nokia Valyrian recovery flow.
#[allow(clippy::module_name_repetitions)]
pub const VALYRIAN: TargetProfile = TargetProfile {
    name: "nokia_valyrian",
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
    recovery_stage_order: &VALYRIAN_RECOVERY_STAGE_ORDER,
    recovery_transfer_order: &VALYRIAN_RECOVERY_TRANSFER_ORDER,
    flash_transfer_order: &VALYRIAN_FLASH_TRANSFER_ORDER,
};

/// Strongly typed target definition for a single supported board profile.
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn validate(self) -> Result<(), RegexError> {
        self.prompts.validate()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn validate(self) -> Result<(), RegexError> {
        self.initial_recovery.compile()?;
        self.second_stage.compile()?;
        self.uboot.compile()?;
        Ok(())
    }
}

/// Raw regex source for a board-specific prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptPattern {
    source: &'static str,
}

impl PromptPattern {
    /// Creates a prompt pattern from a static regex source string.
    #[must_use]
    pub const fn new(source: &'static str) -> Self {
        Self { source }
    }

    /// Returns the raw regex source string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.source
    }

    /// Compiles the prompt regex.
    ///
    /// # Errors
    ///
    /// Returns the `regex` crate's compilation error.
    pub fn compile(self) -> Result<Regex, RegexError> {
        Regex::new(self.source)
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

/// Inclusive-start block range stored as offset + count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    pub start_block: BlockOffset,
    pub block_count: BlockCount,
}

impl BlockRange {
    /// Creates a block range from a start offset and block count.
    #[must_use]
    pub const fn new(start_block: BlockOffset, block_count: BlockCount) -> Self {
        Self {
            start_block,
            block_count,
        }
    }

    /// Returns the exclusive end block.
    #[must_use]
    pub const fn end_block(self) -> BlockOffset {
        BlockOffset::new(self.start_block.get() + self.block_count.get())
    }

    /// Returns the maximum payload size in bytes for this range.
    #[must_use]
    pub const fn byte_len(self, block_size: MmcBlockSize) -> u64 {
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
    pub const fn bytes(self, block_size: MmcBlockSize) -> u64 {
        (self.0 as u64) * (block_size.get() as u64)
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

#[cfg(test)]
mod tests {
    use super::{
        BlockCount, BlockOffset, MmcBlockSize, VALYRIAN, VALYRIAN_FLASH_TRANSFER_ORDER,
        VALYRIAN_RECOVERY_STAGE_ORDER, VALYRIAN_RECOVERY_TRANSFER_ORDER,
    };
    use crate::event::{ImageKind, RecoveryStage, TransferStage};
    use serialport::{DataBits, FlowControl, Parity, StopBits};

    #[test]
    fn valyrian_profile_regexes_compile() {
        assert!(VALYRIAN.validate().is_ok());
    }

    #[test]
    fn valyrian_serial_defaults_match_documented_line_settings() {
        assert_eq!(VALYRIAN.serial.baud_rate, 115_200);
        assert_eq!(VALYRIAN.serial.data_bits, DataBits::Eight);
        assert_eq!(VALYRIAN.serial.parity, Parity::None);
        assert_eq!(VALYRIAN.serial.stop_bits, StopBits::One);
        assert_eq!(VALYRIAN.serial.flow_control, FlowControl::None);
    }

    #[test]
    fn valyrian_flash_layout_matches_the_protocol_doc() {
        assert_eq!(VALYRIAN.flash.block_size, MmcBlockSize::new(512));
        assert_eq!(VALYRIAN.flash.erase_range.start_block, BlockOffset::new(0));
        assert_eq!(
            VALYRIAN.flash.erase_range.block_count,
            BlockCount::new(0x800)
        );
        assert_eq!(VALYRIAN.flash.preloader.start_block, BlockOffset::new(0x4));
        assert_eq!(VALYRIAN.flash.preloader.block_count, BlockCount::new(0xfc));
        assert_eq!(VALYRIAN.flash.fip.start_block, BlockOffset::new(0x100));
        assert_eq!(VALYRIAN.flash.fip.block_count, BlockCount::new(0x700));
    }

    #[test]
    fn valyrian_stage_order_matches_expected_recovery_flow() {
        assert_eq!(
            VALYRIAN.recovery_stage_order,
            &[
                RecoveryStage::Bootrom,
                RecoveryStage::PreloaderPrompt,
                RecoveryStage::FipPrompt,
                RecoveryStage::UBoot,
                RecoveryStage::FlashPlan,
            ]
        );
        assert_eq!(
            VALYRIAN.recovery_transfer_order,
            &[TransferStage::Preloader, TransferStage::Fip]
        );
        assert_eq!(
            VALYRIAN.flash_transfer_order,
            &[TransferStage::LoadxPreloader, TransferStage::LoadxFip]
        );
        assert_eq!(
            VALYRIAN.recovery_stage_order,
            &VALYRIAN_RECOVERY_STAGE_ORDER
        );
        assert_eq!(
            VALYRIAN.recovery_transfer_order,
            &VALYRIAN_RECOVERY_TRANSFER_ORDER
        );
        assert_eq!(
            VALYRIAN.flash_transfer_order,
            &VALYRIAN_FLASH_TRANSFER_ORDER
        );
    }

    #[test]
    fn initial_prompt_pattern_also_matches_the_second_stage_text() {
        let initial_prompt = VALYRIAN.prompts.initial_recovery.compile().unwrap();
        let second_stage = "Press x to load BL31 + U-Boot FIP";

        assert!(initial_prompt.is_match(second_stage));
    }

    #[test]
    fn flash_ranges_return_expected_capacity() {
        assert_eq!(
            VALYRIAN.flash.preloader.end_block(),
            BlockOffset::new(0x100)
        );
        assert_eq!(
            VALYRIAN.flash.preloader.byte_len(VALYRIAN.flash.block_size),
            129_024
        );
        assert_eq!(
            VALYRIAN.flash.fip.byte_len(VALYRIAN.flash.block_size),
            917_504
        );
    }

    #[test]
    fn flash_layout_can_select_ranges_by_image_kind() {
        assert_eq!(
            VALYRIAN.flash.range_for(ImageKind::Preloader),
            VALYRIAN.flash.preloader
        );
        assert_eq!(VALYRIAN.flash.range_for(ImageKind::Fip), VALYRIAN.flash.fip);
    }
}
