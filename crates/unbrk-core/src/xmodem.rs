//! XMODEM-specific helpers that operate on raw UART bytes.

use crate::event::TransferStage;
use crate::transport::Transport;
use std::io;
use std::time::Duration;
use thiserror::Error;

/// The raw byte emitted by the target when it is ready for XMODEM-CRC.
pub const XMODEM_CRC_READY_BYTE: u8 = b'C';

/// Minimum number of readiness bytes required before a transfer may start.
pub const XMODEM_CRC_READY_MIN_BYTES: u32 = 3;

/// Bytes per XMODEM-CRC data block.
pub const XMODEM_BLOCK_SIZE: usize = 128;

/// Start-of-header marker for 128-byte XMODEM packets.
pub const XMODEM_SOH: u8 = 0x01;

/// End-of-transmission marker.
pub const XMODEM_EOT: u8 = 0x04;

/// Positive acknowledgement marker.
pub const XMODEM_ACK: u8 = 0x06;

/// Negative acknowledgement marker.
pub const XMODEM_NAK: u8 = 0x15;

/// Cancel marker.
pub const XMODEM_CAN: u8 = 0x18;

/// Padding byte used for short final blocks.
pub const XMODEM_PADDING_BYTE: u8 = 0x1a;

/// Zero preserves the transport's existing timeout.
pub const XMODEM_DEFAULT_PACKET_TIMEOUT: Duration = Duration::ZERO;

/// Default retry budget for data-block retransmission.
pub const XMODEM_DEFAULT_BLOCK_RETRY_LIMIT: u32 = 10;

/// Default retry budget for the final EOT handshake.
pub const XMODEM_DEFAULT_EOT_RETRY_LIMIT: u32 = 10;

const XMODEM_PACKET_LEN: usize = 3 + XMODEM_BLOCK_SIZE + 2;

/// Configuration for the XMODEM-CRC sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmodemConfig {
    pub packet_timeout: Duration,
    pub block_retry_limit: u32,
    pub eot_retry_limit: u32,
}

impl XmodemConfig {
    #[must_use]
    pub const fn new(
        packet_timeout: Duration,
        block_retry_limit: u32,
        eot_retry_limit: u32,
    ) -> Self {
        Self {
            packet_timeout,
            block_retry_limit,
            eot_retry_limit,
        }
    }

    #[must_use]
    const fn normalized(self) -> Self {
        Self {
            packet_timeout: self.packet_timeout,
            block_retry_limit: if self.block_retry_limit == 0 {
                1
            } else {
                self.block_retry_limit
            },
            eot_retry_limit: if self.eot_retry_limit == 0 {
                1
            } else {
                self.eot_retry_limit
            },
        }
    }
}

impl Default for XmodemConfig {
    fn default() -> Self {
        Self::new(
            XMODEM_DEFAULT_PACKET_TIMEOUT,
            XMODEM_DEFAULT_BLOCK_RETRY_LIMIT,
            XMODEM_DEFAULT_EOT_RETRY_LIMIT,
        )
    }
}

/// Progress update emitted after each acknowledged XMODEM data block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmodemProgress {
    pub stage: TransferStage,
    pub blocks_completed: u32,
    pub bytes_sent: u64,
    pub total_bytes: u64,
}

impl XmodemProgress {
    #[must_use]
    pub const fn new(
        stage: TransferStage,
        blocks_completed: u32,
        bytes_sent: u64,
        total_bytes: u64,
    ) -> Self {
        Self {
            stage,
            blocks_completed,
            bytes_sent,
            total_bytes,
        }
    }
}

/// Final summary returned by the XMODEM-CRC sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmodemTransferReport {
    pub stage: TransferStage,
    pub blocks_sent: u32,
    pub bytes_sent: u64,
    pub total_bytes: u64,
    pub eot_attempts: u32,
}

impl XmodemTransferReport {
    #[must_use]
    const fn new(
        stage: TransferStage,
        blocks_sent: u32,
        bytes_sent: u64,
        total_bytes: u64,
        eot_attempts: u32,
    ) -> Self {
        Self {
            stage,
            blocks_sent,
            bytes_sent,
            total_bytes,
            eot_attempts,
        }
    }
}

/// Sender failures for XMODEM-CRC transfers.
#[derive(Debug, Error)]
pub enum XmodemError {
    #[error("I/O failed while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("XMODEM transfers require at least one data byte")]
    EmptyPayload,
    #[error("timed out while waiting for {operation}")]
    Timeout { operation: &'static str },
    #[error("receiver rejected block {block_number} after {attempts} attempt(s)")]
    RetryLimitExceeded { block_number: u8, attempts: u32 },
    #[error("receiver rejected EOT after {attempts} attempt(s)")]
    EotRetryLimitExceeded { attempts: u32 },
    #[error("receiver cancelled the transfer while waiting for {operation}")]
    ReceiverCancelled { operation: &'static str },
    #[error("unexpected XMODEM response while waiting for {operation}: 0x{byte:02x}")]
    UnexpectedResponse { operation: &'static str, byte: u8 },
}

impl XmodemError {
    /// Returns whether a prompt observed immediately after transfer failure can
    /// be treated as successful completion.
    #[must_use]
    pub(crate) fn permits_prompt_completion_recovery(&self) -> bool {
        matches!(
            self,
            Self::EotRetryLimitExceeded { .. }
                | Self::Timeout {
                    operation: "EOT ACK/NAK",
                }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverResponse {
    Ack,
    Nak,
    Cancel,
}

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

/// Computes CRC-16/XMODEM over `bytes`.
#[must_use]
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;

    for byte in bytes {
        crc ^= u16::from(*byte) << 8;

        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }

    crc
}

/// Builds a single XMODEM-CRC packet for `payload`.
///
/// Short payloads are padded with [`XMODEM_PADDING_BYTE`] to reach a full
/// 128-byte XMODEM block.
///
/// # Panics
///
/// Panics when `payload` is longer than [`XMODEM_BLOCK_SIZE`].
#[must_use]
pub fn build_crc_packet(sequence: u8, payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() <= XMODEM_BLOCK_SIZE,
        "XMODEM payload chunks must be at most 128 bytes",
    );

    let mut data = [XMODEM_PADDING_BYTE; XMODEM_BLOCK_SIZE];
    data[..payload.len()].copy_from_slice(payload);

    let crc = crc16_xmodem(&data);
    let mut packet = Vec::with_capacity(XMODEM_PACKET_LEN);
    packet.push(XMODEM_SOH);
    packet.push(sequence);
    packet.push(!sequence);
    packet.extend_from_slice(&data);
    packet.extend_from_slice(&crc.to_be_bytes());
    packet
}

/// Sends `payload` using XMODEM-CRC over `transport`.
///
/// The sender retransmits a block when the receiver responds with NAK and
/// retransmits EOT when the receiver NAKs the final handshake.
///
/// # Errors
///
/// Returns [`XmodemError`] when transport I/O fails, the receiver times out,
/// or the receiver rejects the transfer beyond the configured retry budget.
///
/// If `config.packet_timeout` is non-zero, it is applied to the transport
/// before the transfer starts.
pub fn send_crc<T, F>(
    transport: &mut T,
    stage: TransferStage,
    payload: &[u8],
    config: XmodemConfig,
    mut on_progress: F,
) -> Result<XmodemTransferReport, XmodemError>
where
    T: Transport,
    F: FnMut(XmodemProgress),
{
    let config = config.normalized();

    if payload.is_empty() {
        return Err(XmodemError::EmptyPayload);
    }

    if !config.packet_timeout.is_zero() {
        transport
            .set_timeout(config.packet_timeout)
            .map_err(|source| XmodemError::Io {
                operation: "configure XMODEM timeout",
                source,
            })?;
    }

    let total_bytes = usize_to_u64(payload.len());
    let mut blocks_sent = 0_u32;

    for (block_index, chunk) in payload.chunks(XMODEM_BLOCK_SIZE).enumerate() {
        let sequence = block_sequence(block_index);
        let packet = build_crc_packet(sequence, chunk);
        let mut attempts = 0_u32;

        loop {
            attempts += 1;
            write_frame(transport, &packet, "send XMODEM block")?;

            match read_receiver_response(transport, "block ACK/NAK")? {
                ReceiverResponse::Ack => {
                    blocks_sent += 1;
                    let block_start = usize_to_u64(block_index).saturating_mul(128_u64);
                    let acknowledged_bytes =
                        total_bytes.min(block_start + usize_to_u64(chunk.len()));

                    on_progress(XmodemProgress::new(
                        stage,
                        blocks_sent,
                        acknowledged_bytes,
                        total_bytes,
                    ));
                    break;
                }
                ReceiverResponse::Nak if attempts < config.block_retry_limit => {}
                ReceiverResponse::Nak => {
                    return Err(XmodemError::RetryLimitExceeded {
                        block_number: sequence,
                        attempts,
                    });
                }
                ReceiverResponse::Cancel => {
                    return Err(XmodemError::ReceiverCancelled {
                        operation: "block ACK/NAK",
                    });
                }
            }
        }
    }

    let eot_attempts = finish_eot_handshake(transport, config.eot_retry_limit)?;

    Ok(XmodemTransferReport::new(
        stage,
        blocks_sent,
        total_bytes,
        total_bytes,
        eot_attempts,
    ))
}

#[must_use]
fn block_sequence(block_index: usize) -> u8 {
    let sequence_space = usize::from(u8::MAX) + 1;
    u8::try_from((block_index + 1) % sequence_space).unwrap_or_default()
}

#[must_use]
fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn write_frame<T: Transport>(
    transport: &mut T,
    bytes: &[u8],
    operation: &'static str,
) -> Result<(), XmodemError> {
    transport
        .write(bytes)
        .map_err(|source| XmodemError::Io { operation, source })?;
    transport
        .flush()
        .map_err(|source| XmodemError::Io { operation, source })
}

fn read_receiver_response<T: Transport>(
    transport: &mut T,
    operation: &'static str,
) -> Result<ReceiverResponse, XmodemError> {
    match transport.read_byte() {
        Ok(Some(XMODEM_ACK)) => Ok(ReceiverResponse::Ack),
        Ok(Some(XMODEM_NAK)) => Ok(ReceiverResponse::Nak),
        Ok(Some(XMODEM_CAN)) => Ok(ReceiverResponse::Cancel),
        Ok(Some(byte)) => Err(XmodemError::UnexpectedResponse { operation, byte }),
        Ok(None) => Err(XmodemError::Timeout { operation }),
        Err(source) if source.kind() == io::ErrorKind::TimedOut => {
            Err(XmodemError::Timeout { operation })
        }
        Err(source) => Err(XmodemError::Io { operation, source }),
    }
}

fn finish_eot_handshake<T: Transport>(
    transport: &mut T,
    retry_limit: u32,
) -> Result<u32, XmodemError> {
    let mut attempts = 0_u32;

    loop {
        attempts += 1;
        write_frame(transport, &[XMODEM_EOT], "send EOT")?;

        match read_receiver_response(transport, "EOT ACK/NAK")? {
            ReceiverResponse::Ack => return Ok(attempts),
            ReceiverResponse::Nak if attempts < retry_limit => {}
            ReceiverResponse::Nak => {
                return Err(XmodemError::EotRetryLimitExceeded { attempts });
            }
            ReceiverResponse::Cancel => {
                return Err(XmodemError::ReceiverCancelled {
                    operation: "EOT ACK/NAK",
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CrcReadyMatch, XMODEM_ACK, XMODEM_BLOCK_SIZE, XMODEM_CAN, XMODEM_CRC_READY_BYTE,
        XMODEM_CRC_READY_MIN_BYTES, XMODEM_EOT, XMODEM_NAK, XMODEM_PADDING_BYTE, XMODEM_SOH,
        XmodemConfig, XmodemError, XmodemProgress, XmodemTransferReport, advance_to_crc_ready,
        build_crc_packet, crc16_xmodem, find_crc_ready, send_crc,
    };
    use crate::event::TransferStage;
    use crate::transport::{MockStep, MockTransport};
    use std::io;
    use std::time::Duration;

    const STAGE1_CRC_READY: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/happy-path-stage1-crc-readiness.bin");
    const STAGE2_CRC_READY: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/happy-path-stage2-crc-readiness.bin");
    const REAL_PRELOADER_ECHOED_X_CRC: &[u8] =
        include_bytes!("../../../tests/fixtures/an7581/real-preloader-echoed-x-crc.bin");
    const PACKET_LEN: usize = 3 + XMODEM_BLOCK_SIZE + 2;
    const TEST_PACKET_TIMEOUT: Duration = Duration::from_millis(250);

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
    fn crc_ready_can_follow_an_echoed_input_byte_from_real_hardware() {
        let readiness = find_crc_ready(REAL_PRELOADER_ECHOED_X_CRC, 0).unwrap();

        assert_eq!(
            readiness,
            CrcReadyMatch {
                readiness_bytes_seen: XMODEM_CRC_READY_MIN_BYTES,
                next_cursor: 4,
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

    #[test]
    fn crc16_matches_the_standard_reference_vector() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
    }

    #[test]
    fn packet_builder_pads_short_blocks_and_appends_crc() {
        let packet = build_crc_packet(1, b"123456789");

        assert_eq!(packet.len(), PACKET_LEN);
        assert_eq!(packet[0], XMODEM_SOH);
        assert_eq!(packet[1], 1);
        assert_eq!(packet[2], !1);
        assert_eq!(&packet[3..12], b"123456789");
        assert!(
            packet[12..(3 + XMODEM_BLOCK_SIZE)]
                .iter()
                .all(|byte| *byte == XMODEM_PADDING_BYTE)
        );
        assert_eq!(packet[PACKET_LEN - 2], 0xe4);
        assert_eq!(packet[PACKET_LEN - 1], 0x47);
    }

    #[test]
    fn send_crc_updates_the_transport_timeout_when_configured() {
        let payload = [0x42];
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::SetTimeout(TEST_PACKET_TIMEOUT),
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);

        let report = send_crc(
            &mut transport,
            TransferStage::Preloader,
            &payload,
            XmodemConfig::new(TEST_PACKET_TIMEOUT, 1, 1),
            |_| {},
        )
        .unwrap();

        assert_eq!(report.blocks_sent, 1);
        assert_eq!(report.eot_attempts, 1);
        assert_eq!(transport.timeout_updates(), &[TEST_PACKET_TIMEOUT]);
        transport.assert_finished();
    }

    #[test]
    fn send_crc_transfers_a_small_file_and_reports_progress() {
        let payload = b"hello".to_vec();
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);
        let mut progress = Vec::new();

        let report = send_crc(
            &mut transport,
            TransferStage::Preloader,
            &payload,
            XmodemConfig::default(),
            |update| progress.push(update),
        )
        .unwrap();

        assert_eq!(
            report,
            XmodemTransferReport {
                stage: TransferStage::Preloader,
                blocks_sent: 1,
                bytes_sent: 5,
                total_bytes: 5,
                eot_attempts: 1,
            }
        );
        assert_eq!(
            progress,
            vec![XmodemProgress::new(TransferStage::Preloader, 1, 5, 5)]
        );
        transport.assert_finished();
    }

    #[test]
    fn send_crc_retransmits_the_same_block_after_nak() {
        let payload = vec![0x42; XMODEM_BLOCK_SIZE];
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet.clone()),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_NAK]),
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);
        let mut progress = Vec::new();

        let report = send_crc(
            &mut transport,
            TransferStage::Fip,
            &payload,
            XmodemConfig::new(Duration::ZERO, 2, 1),
            |update| progress.push(update),
        )
        .unwrap();

        assert_eq!(report.blocks_sent, 1);
        assert_eq!(report.bytes_sent, 128);
        assert_eq!(
            progress,
            vec![XmodemProgress::new(TransferStage::Fip, 1, 128, 128)]
        );
        transport.assert_finished();
    }

    #[test]
    fn send_crc_retries_eot_until_the_receiver_acknowledges() {
        let payload = vec![0xaa; 3];
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_NAK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);

        let report = send_crc(
            &mut transport,
            TransferStage::LoadxPreloader,
            &payload,
            XmodemConfig::new(Duration::ZERO, 1, 2),
            |_| {},
        )
        .unwrap();

        assert_eq!(report.eot_attempts, 2);
        transport.assert_finished();
    }

    #[test]
    fn send_crc_times_out_when_an_ack_does_not_arrive() {
        let payload = b"timeout".to_vec();
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::ReadError {
                kind: io::ErrorKind::TimedOut,
                message: String::from("serial read timed out"),
            },
        ]);

        let error = send_crc(
            &mut transport,
            TransferStage::Preloader,
            &payload,
            XmodemConfig::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            XmodemError::Timeout {
                operation: "block ACK/NAK",
            }
        ));
    }

    #[test]
    fn prompt_recovery_is_allowed_only_for_eot_completion_failures() {
        assert!(
            XmodemError::EotRetryLimitExceeded { attempts: 1 }.permits_prompt_completion_recovery()
        );
        assert!(
            XmodemError::Timeout {
                operation: "EOT ACK/NAK",
            }
            .permits_prompt_completion_recovery()
        );

        assert!(
            !XmodemError::Timeout {
                operation: "block ACK/NAK",
            }
            .permits_prompt_completion_recovery()
        );
        assert!(
            !XmodemError::RetryLimitExceeded {
                block_number: 1,
                attempts: 3,
            }
            .permits_prompt_completion_recovery()
        );
        assert!(
            !XmodemError::ReceiverCancelled {
                operation: "EOT ACK/NAK",
            }
            .permits_prompt_completion_recovery()
        );
        assert!(
            !XmodemError::UnexpectedResponse {
                operation: "EOT ACK/NAK",
                byte: b'P',
            }
            .permits_prompt_completion_recovery()
        );
        assert!(
            !XmodemError::Io {
                operation: "send EOT",
                source: io::Error::other("boom"),
            }
            .permits_prompt_completion_recovery()
        );
    }

    #[test]
    fn send_crc_handles_an_exact_block_boundary() {
        let payload = vec![0x55; XMODEM_BLOCK_SIZE];
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);
        let mut progress = Vec::new();

        let report = send_crc(
            &mut transport,
            TransferStage::Fip,
            &payload,
            XmodemConfig::default(),
            |update| progress.push(update),
        )
        .unwrap();

        assert_eq!(report.blocks_sent, 1);
        assert_eq!(
            progress,
            vec![XmodemProgress::new(TransferStage::Fip, 1, 128, 128)]
        );
        transport.assert_finished();
    }

    #[test]
    fn send_crc_handles_a_payload_one_byte_over_the_boundary() {
        let payload = vec![0x7f; XMODEM_BLOCK_SIZE + 1];
        let first_packet = build_crc_packet(1, &payload[..XMODEM_BLOCK_SIZE]);
        let second_packet = build_crc_packet(2, &payload[XMODEM_BLOCK_SIZE..]);
        let mut transport = MockTransport::new([
            MockStep::Write(first_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(second_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
            MockStep::Write(vec![XMODEM_EOT]),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_ACK]),
        ]);
        let mut progress = Vec::new();

        let report = send_crc(
            &mut transport,
            TransferStage::LoadxFip,
            &payload,
            XmodemConfig::default(),
            |update| progress.push(update),
        )
        .unwrap();

        assert_eq!(report.blocks_sent, 2);
        assert_eq!(
            progress,
            vec![
                XmodemProgress::new(TransferStage::LoadxFip, 1, 128, 129),
                XmodemProgress::new(TransferStage::LoadxFip, 2, 129, 129),
            ]
        );
        transport.assert_finished();
    }

    #[test]
    fn send_crc_maps_cancel_to_a_receiver_cancelled_error() {
        let payload = b"stop".to_vec();
        let expected_packet = build_crc_packet(1, &payload);
        let mut transport = MockTransport::new([
            MockStep::Write(expected_packet),
            MockStep::Flush,
            MockStep::Read(vec![XMODEM_CAN]),
        ]);

        let error = send_crc(
            &mut transport,
            TransferStage::Preloader,
            &payload,
            XmodemConfig::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            XmodemError::ReceiverCancelled {
                operation: "block ACK/NAK",
            }
        ));
    }

    #[test]
    fn send_crc_rejects_empty_payloads() {
        let mut transport = MockTransport::new([]);

        let error = send_crc(
            &mut transport,
            TransferStage::Preloader,
            &[],
            XmodemConfig::default(),
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, XmodemError::EmptyPayload));
        assert!(transport.writes().is_empty());
        transport.assert_finished();
    }
}
