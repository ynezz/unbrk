//! XMODEM-specific helpers that operate on raw UART bytes.

/// The raw byte emitted by the target when it is ready for XMODEM-CRC.
pub const XMODEM_CRC_READY_BYTE: u8 = b'C';

/// Minimum number of readiness bytes required before a transfer may start.
pub const XMODEM_CRC_READY_MIN_BYTES: u32 = 3;

/// Result of finding an XMODEM-CRC readiness marker in a UART buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrcReadyMatch {
    pub readiness_bytes_seen: u32,
    pub next_cursor: usize,
}

impl CrcReadyMatch {
    #[must_use]
    const fn new(readiness_bytes_seen: u32, next_cursor: usize) -> Self {
        Self {
            readiness_bytes_seen,
            next_cursor,
        }
    }
}

/// Finds the next XMODEM-CRC readiness marker in `buffer` from `cursor`.
///
/// Readiness is emitted as repeated `C` bytes. The detector tolerates ASCII
/// control-byte noise between those `C` bytes because the observed UART stream
/// can interleave non-printable bytes with the readiness marker.
#[must_use]
pub fn find_crc_ready(buffer: &[u8], cursor: usize) -> Option<CrcReadyMatch> {
    let bytes = buffer.get(cursor..)?;
    let mut readiness_bytes_seen = 0_u32;

    for (offset, byte) in bytes.iter().copied().enumerate() {
        if byte == XMODEM_CRC_READY_BYTE {
            readiness_bytes_seen += 1;

            if readiness_bytes_seen >= XMODEM_CRC_READY_MIN_BYTES {
                return Some(CrcReadyMatch::new(
                    readiness_bytes_seen,
                    cursor + offset + 1,
                ));
            }

            continue;
        }

        if byte.is_ascii_control() {
            continue;
        }

        readiness_bytes_seen = 0;
    }

    None
}

/// Advances `cursor` past the next readiness marker if one is present.
///
/// This mirrors the project-wide cursor discipline: once a stage consumes a
/// marker, subsequent stages should only inspect newer bytes.
pub fn advance_to_crc_ready(buffer: &[u8], cursor: &mut usize) -> Option<CrcReadyMatch> {
    let readiness = find_crc_ready(buffer, *cursor)?;
    *cursor = readiness.next_cursor;
    Some(readiness)
}

#[cfg(test)]
mod tests {
    use super::{
        CrcReadyMatch, XMODEM_CRC_READY_BYTE, XMODEM_CRC_READY_MIN_BYTES, advance_to_crc_ready,
        find_crc_ready,
    };

    const STAGE1_CRC_READY: &[u8] =
        include_bytes!("../../../tests/fixtures/valyrian/happy-path-stage1-crc-readiness.bin");
    const STAGE2_CRC_READY: &[u8] =
        include_bytes!("../../../tests/fixtures/valyrian/happy-path-stage2-crc-readiness.bin");

    #[test]
    fn finds_a_plain_crc_ready_triplet() {
        let readiness = find_crc_ready(b"CCC", 0).unwrap();

        assert_eq!(
            readiness,
            CrcReadyMatch {
                readiness_bytes_seen: XMODEM_CRC_READY_MIN_BYTES,
                next_cursor: 3,
            }
        );
    }

    #[test]
    fn tolerates_control_bytes_between_crc_ready_bytes() {
        let buffer = [
            0x18,
            XMODEM_CRC_READY_BYTE,
            0x00,
            XMODEM_CRC_READY_BYTE,
            b'\r',
            XMODEM_CRC_READY_BYTE,
            b'>',
        ];

        let readiness = find_crc_ready(&buffer, 0).unwrap();
        assert_eq!(
            readiness,
            CrcReadyMatch {
                readiness_bytes_seen: XMODEM_CRC_READY_MIN_BYTES,
                next_cursor: 6,
            }
        );
    }

    #[test]
    fn printable_bytes_reset_the_crc_ready_run() {
        let readiness = find_crc_ready(b"C\x00CxCCC", 0).unwrap();

        assert_eq!(
            readiness,
            CrcReadyMatch {
                readiness_bytes_seen: XMODEM_CRC_READY_MIN_BYTES,
                next_cursor: 7,
            }
        );
    }

    #[test]
    fn advancing_the_cursor_prevents_rematching_stale_bytes() {
        let mut cursor = 0;
        let buffer = b"CCCnoiseCCC";

        let first = advance_to_crc_ready(buffer, &mut cursor).unwrap();
        assert_eq!(first.next_cursor, 3);
        assert_eq!(cursor, 3);

        let second = advance_to_crc_ready(buffer, &mut cursor).unwrap();
        assert_eq!(second.next_cursor, buffer.len());
        assert_eq!(cursor, buffer.len());
    }

    #[test]
    fn incomplete_sequences_do_not_advance_the_cursor() {
        let mut cursor = 0;

        assert_eq!(advance_to_crc_ready(b"C\x00C", &mut cursor), None);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn out_of_bounds_cursor_does_not_match() {
        assert_eq!(find_crc_ready(b"CCC", 4), None);
    }

    #[test]
    fn stage1_fixture_reports_crc_ready_from_raw_bytes() {
        let readiness = find_crc_ready(STAGE1_CRC_READY, 0).unwrap();

        assert_eq!(readiness.readiness_bytes_seen, XMODEM_CRC_READY_MIN_BYTES);
        assert_eq!(readiness.next_cursor, 3);
    }

    #[test]
    fn cursor_can_skip_a_stale_crc_burst_and_find_the_next_fixture() {
        let mut combined = Vec::new();
        combined.extend_from_slice(STAGE1_CRC_READY);
        combined.extend_from_slice(b"boot chatter\r\n");
        combined.extend_from_slice(STAGE2_CRC_READY);

        let mut cursor = 0;
        let first = advance_to_crc_ready(&combined, &mut cursor).unwrap();
        assert_eq!(first.next_cursor, 3);

        let second = advance_to_crc_ready(&combined, &mut cursor).unwrap();
        assert_eq!(
            second,
            CrcReadyMatch {
                readiness_bytes_seen: XMODEM_CRC_READY_MIN_BYTES,
                next_cursor: STAGE1_CRC_READY.len() + b"boot chatter\r\n".len() + 3,
            }
        );
    }
}
