//! Prompt matching helpers over accumulated raw console bytes.

use crate::target::PromptPattern;
use regex::{Error as RegexError, bytes::Regex};

/// Successful prompt match with the next cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMatch {
    pub prompt: String,
    pub next_cursor: usize,
}

impl PromptMatch {
    #[must_use]
    fn new(bytes: &[u8], next_cursor: usize) -> Self {
        Self {
            prompt: String::from_utf8_lossy(bytes).into_owned(),
            next_cursor,
        }
    }
}

fn find_prompt_impl(
    regex: &Regex,
    buffer: &[u8],
    cursor: usize,
    allow_trailing_space: bool,
) -> Option<PromptMatch> {
    let bytes = buffer.get(cursor..)?;

    for matched in regex.find_iter(bytes) {
        let trailing = bytes.get(matched.end()).copied();
        if trailing
            .is_none_or(|byte| byte.is_ascii_control() || (allow_trailing_space && byte == b' '))
        {
            return Some(PromptMatch::new(
                &bytes[matched.start()..matched.end()],
                cursor + matched.end(),
            ));
        }
    }

    None
}

#[must_use]
pub fn find_prompt_with_regex(regex: &Regex, buffer: &[u8], cursor: usize) -> Option<PromptMatch> {
    find_prompt_impl(regex, buffer, cursor, false)
}

#[must_use]
pub fn find_prompt_allowing_trailing_space_with_regex(
    regex: &Regex,
    buffer: &[u8],
    cursor: usize,
) -> Option<PromptMatch> {
    find_prompt_impl(regex, buffer, cursor, true)
}

/// Finds the next stage-local prompt match from `cursor`.
///
/// The matcher accepts the first regex hit whose trailing byte is either absent
/// or an ASCII control byte. That prevents the short `Press x` prompt from
/// matching the longer `Press x to load BL31 + U-Boot FIP` prompt out of
/// sequence while still tolerating CR, LF, and CRLF endings.
///
/// # Errors
///
/// Returns a regex compilation error when the prompt source is invalid.
pub fn find_prompt(
    pattern: PromptPattern,
    buffer: &[u8],
    cursor: usize,
) -> Result<Option<PromptMatch>, RegexError> {
    let regex = pattern.compile()?;
    Ok(find_prompt_with_regex(&regex, buffer, cursor))
}

/// Finds the next prompt match from `cursor`, also accepting a trailing space.
///
/// U-Boot prompts commonly render as `AN7581> `, so callers that wait for the
/// live shell prompt need to tolerate a single ASCII space after the regex hit.
///
/// # Errors
///
/// Returns a regex compilation error when the prompt source is invalid.
pub fn find_prompt_allowing_trailing_space(
    pattern: PromptPattern,
    buffer: &[u8],
    cursor: usize,
) -> Result<Option<PromptMatch>, RegexError> {
    let regex = pattern.compile()?;
    Ok(find_prompt_allowing_trailing_space_with_regex(
        &regex, buffer, cursor,
    ))
}

/// Advances `cursor` past the next matched prompt if one is present.
///
/// # Errors
///
/// Returns a regex compilation error when the prompt source is invalid.
pub fn advance_to_prompt(
    pattern: PromptPattern,
    buffer: &[u8],
    cursor: &mut usize,
) -> Result<Option<PromptMatch>, RegexError> {
    let regex = pattern.compile()?;
    let matched = advance_to_prompt_with_regex(&regex, buffer, cursor);

    Ok(matched)
}

pub fn advance_to_prompt_with_regex(
    regex: &Regex,
    buffer: &[u8],
    cursor: &mut usize,
) -> Option<PromptMatch> {
    let matched = find_prompt_with_regex(regex, buffer, *cursor);

    if let Some(ref matched) = matched {
        *cursor = matched.next_cursor;
    }

    matched
}

/// Advances `cursor` past the next matched prompt, allowing a trailing space.
///
/// # Errors
///
/// Returns a regex compilation error when the prompt source is invalid.
pub fn advance_to_prompt_allowing_trailing_space(
    pattern: PromptPattern,
    buffer: &[u8],
    cursor: &mut usize,
) -> Result<Option<PromptMatch>, RegexError> {
    let regex = pattern.compile()?;
    let matched = advance_to_prompt_allowing_trailing_space_with_regex(&regex, buffer, cursor);

    Ok(matched)
}

pub fn advance_to_prompt_allowing_trailing_space_with_regex(
    regex: &Regex,
    buffer: &[u8],
    cursor: &mut usize,
) -> Option<PromptMatch> {
    let matched = find_prompt_allowing_trailing_space_with_regex(regex, buffer, *cursor);

    if let Some(ref matched) = matched {
        *cursor = matched.next_cursor;
    }

    matched
}

#[cfg(test)]
mod tests {
    use super::{
        PromptMatch, advance_to_prompt, advance_to_prompt_allowing_trailing_space,
        advance_to_prompt_allowing_trailing_space_with_regex, find_prompt,
        find_prompt_allowing_trailing_space,
    };
    use crate::target::AN7581;

    const STAGE1_PROMPT: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/happy-path-stage1-prompt.bin");
    const STAGE2_PROMPT: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/happy-path-stage2-prompt.bin");
    const REAL_STAGE1_LEADING_GARBAGE: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/real-stage1-leading-garbage.bin");
    const REAL_STAGE2_NOTICE_AND_PROMPT: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/real-stage2-notice-and-prompt.bin");
    const REAL_UBOOT_ANSI_PROMPT: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/real-uboot-ansi-prompt.bin");

    #[test]
    fn initial_prompt_matches_the_stage1_fixture() {
        let matched = find_prompt(AN7581.prompts.initial_recovery, STAGE1_PROMPT, 0)
            .unwrap()
            .unwrap();

        assert_eq!(
            matched,
            PromptMatch {
                prompt: String::from("Press x"),
                next_cursor: 7,
            }
        );
    }

    #[test]
    fn initial_prompt_does_not_consume_the_longer_stage2_prompt() {
        let matched = find_prompt(AN7581.prompts.initial_recovery, STAGE2_PROMPT, 0).unwrap();

        assert_eq!(matched, None);
    }

    #[test]
    fn second_prompt_matches_the_stage2_fixture() {
        let matched = find_prompt(AN7581.prompts.second_stage, STAGE2_PROMPT, 0)
            .unwrap()
            .unwrap();

        assert_eq!(
            matched,
            PromptMatch {
                prompt: String::from("Press x to load BL31 + U-Boot FIP"),
                next_cursor: 33,
            }
        );
    }

    #[test]
    fn fragmented_prompt_requires_more_bytes_before_matching() {
        let prefix = &STAGE2_PROMPT[..12];
        let full = STAGE2_PROMPT;

        assert_eq!(
            find_prompt(AN7581.prompts.second_stage, prefix, 0).unwrap(),
            None
        );

        let matched = find_prompt(AN7581.prompts.second_stage, full, 0)
            .unwrap()
            .unwrap();
        assert_eq!(matched.next_cursor, 33);
    }

    #[test]
    fn prompt_matching_accepts_control_terminated_lines() {
        let matched = find_prompt(AN7581.prompts.initial_recovery, b"noise\rPress x\n", 0)
            .unwrap()
            .unwrap();

        assert_eq!(matched.prompt, "Press x");
        assert_eq!(matched.next_cursor, 13);
    }

    #[test]
    fn initial_prompt_can_follow_stale_bytes_from_real_hardware() {
        let matched = find_prompt(
            AN7581.prompts.initial_recovery,
            REAL_STAGE1_LEADING_GARBAGE,
            0,
        )
        .unwrap()
        .unwrap();

        assert_eq!(matched.prompt, "Press x");
        assert_eq!(matched.next_cursor, 14);
    }

    #[test]
    fn second_prompt_can_follow_real_boot_notice_chatter() {
        let matched = find_prompt(
            AN7581.prompts.second_stage,
            REAL_STAGE2_NOTICE_AND_PROMPT,
            0,
        )
        .unwrap()
        .unwrap();

        assert_eq!(matched.prompt, "Press x to load BL31 + U-Boot FIP");
        assert_eq!(matched.next_cursor, 52);
    }

    #[test]
    fn uboot_prompt_matching_accepts_a_trailing_space() {
        let matched = find_prompt_allowing_trailing_space(AN7581.prompts.uboot, b"AN7581> ", 0)
            .unwrap()
            .unwrap();

        assert_eq!(matched.prompt, "AN7581>");
        assert_eq!(matched.next_cursor, 7);
    }

    #[test]
    fn uboot_prompt_matching_tolerates_real_ansi_prefix_bytes() {
        let matched =
            find_prompt_allowing_trailing_space(AN7581.prompts.uboot, REAL_UBOOT_ANSI_PROMPT, 0)
                .unwrap()
                .unwrap();

        assert_eq!(matched.prompt, "AN7581>");
        assert_eq!(matched.next_cursor, 23);
    }

    #[test]
    fn cursor_advancement_skips_stale_prompt_text() {
        let mut combined = Vec::new();
        combined.extend_from_slice(STAGE1_PROMPT);
        combined.extend_from_slice(b"DRAM init\r\n");
        combined.extend_from_slice(STAGE2_PROMPT);

        let mut cursor = 0;
        let first = advance_to_prompt(AN7581.prompts.initial_recovery, &combined, &mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(first.prompt, "Press x");
        assert_eq!(cursor, 7);

        let second = advance_to_prompt(AN7581.prompts.second_stage, &combined, &mut cursor)
            .unwrap()
            .unwrap();
        assert_eq!(second.prompt, "Press x to load BL31 + U-Boot FIP");
        assert_eq!(cursor, STAGE1_PROMPT.len() + b"DRAM init\r\n".len() + 33);
    }

    #[test]
    fn cursor_advancement_for_uboot_prompt_stops_before_the_trailing_space() {
        let mut cursor = 0;
        let matched = advance_to_prompt_allowing_trailing_space(
            AN7581.prompts.uboot,
            b"AN7581> \r\n",
            &mut cursor,
        )
        .unwrap()
        .unwrap();

        assert_eq!(matched.prompt, "AN7581>");
        assert_eq!(cursor, 7);
    }

    #[test]
    fn compiled_helper_reuses_the_same_regex_for_cursor_advancement() {
        let regex = AN7581.prompts.uboot.compile().unwrap();
        let mut cursor = 0;

        let first = advance_to_prompt_allowing_trailing_space_with_regex(
            &regex,
            b"AN7581> \r\nAN7581> \r\n",
            &mut cursor,
        )
        .unwrap();

        assert_eq!(first.prompt, "AN7581>");
        assert_eq!(cursor, 7);

        let second = advance_to_prompt_allowing_trailing_space_with_regex(
            &regex,
            b"AN7581> \r\nAN7581> \r\n",
            &mut cursor,
        )
        .unwrap();

        assert_eq!(second.prompt, "AN7581>");
        assert_eq!(cursor, 17);
    }
}
