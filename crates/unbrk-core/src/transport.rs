//! Blocking serial transport abstractions.

use serialport::{DataBits, FlowControl, Parity, StopBits};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::Duration;

/// Default baud rate for the documented Valyrian recovery flow.
pub const DEFAULT_BAUD_RATE: u32 = 115_200;

/// Real serial transport backed by the `serialport` crate.
pub struct SerialTransport {
    path: String,
    port: serialport::SerialPort,
}

impl SerialTransport {
    /// Opens a serial port with explicit line settings.
    ///
    /// The recovery flow uses 8N1 with no flow control on all supported hosts.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the device cannot be opened or configured.
    pub fn open(path: impl Into<String>, baud_rate: u32, timeout: Duration) -> io::Result<Self> {
        let path = path.into();
        let port = serial_port_builder(path.as_str(), baud_rate, timeout)
            .open(path.as_str())
            .map_err(|error| map_serialport_error(path.as_str(), &error))?;

        Ok(Self { path, port })
    }

    /// Opens a serial port using the default Valyrian baud rate.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the device cannot be opened or configured.
    pub fn open_default(path: impl Into<String>, timeout: Duration) -> io::Result<Self> {
        Self::open(path, DEFAULT_BAUD_RATE, timeout)
    }

    /// Returns the configured serial-port path.
    #[must_use]
    pub const fn path(&self) -> &str {
        self.path.as_str()
    }
}

/// Scripted transport step for [`MockTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockStep {
    /// Return these bytes from the next read operation.
    Read(Vec<u8>),
    /// Delay inbound bytes without sleeping wall-clock time.
    Delay(Duration),
    /// Return an error from the next read operation.
    ReadError {
        kind: io::ErrorKind,
        message: String,
    },
    /// Expect the next write operation to match these bytes exactly.
    Write(Vec<u8>),
    /// Return an error from the next write operation.
    WriteError {
        kind: io::ErrorKind,
        message: String,
    },
    /// Expect a flush operation.
    Flush,
    /// Return an error from the next flush operation.
    FlushError {
        kind: io::ErrorKind,
        message: String,
    },
    /// Expect a timeout update to the provided value.
    SetTimeout(Duration),
}

/// Scripted transport implementation for transport-agnostic tests.
///
/// This lets unit and integration tests replay captured UART bytes, split
/// prompts across multiple reads, inject timeout/error conditions, and assert
/// expected writes without touching real hardware.
#[derive(Debug, Default)]
pub struct MockTransport {
    script: VecDeque<MockStep>,
    pending_read: VecDeque<u8>,
    pending_delay: Option<Duration>,
    timeout: Duration,
    writes: Vec<Vec<u8>>,
    reads: Vec<u8>,
    flush_count: usize,
    timeout_updates: Vec<Duration>,
}

impl MockTransport {
    /// Creates a scripted transport from ordered steps.
    #[must_use]
    pub fn new(steps: impl IntoIterator<Item = MockStep>) -> Self {
        Self {
            script: steps.into_iter().collect(),
            pending_read: VecDeque::new(),
            pending_delay: None,
            timeout: Duration::from_secs(1),
            writes: Vec::new(),
            reads: Vec::new(),
            flush_count: 0,
            timeout_updates: Vec::new(),
        }
    }

    /// Creates a transport that only scripts read chunks.
    #[must_use]
    pub fn from_reads(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self::new(chunks.into_iter().map(Self::read_step))
    }

    /// Creates a transport that only scripts inbound byte chunks.
    #[must_use]
    pub fn from_rx_chunks(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self::from_reads(chunks)
    }

    /// Returns the currently configured timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the write operations observed so far.
    #[must_use]
    pub const fn writes(&self) -> &[Vec<u8>] {
        self.writes.as_slice()
    }

    /// Returns the bytes delivered through successful read operations.
    #[must_use]
    pub const fn rx_log(&self) -> &[u8] {
        self.reads.as_slice()
    }

    /// Returns the number of observed flush calls.
    #[must_use]
    pub const fn flush_count(&self) -> usize {
        self.flush_count
    }

    /// Returns the timeout updates observed so far.
    #[must_use]
    pub const fn timeout_updates(&self) -> &[Duration] {
        self.timeout_updates.as_slice()
    }

    /// Returns whether the scripted steps and buffered read bytes are exhausted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.script.is_empty() && self.pending_read.is_empty() && self.pending_delay.is_none()
    }

    /// Asserts that the scripted transport was fully consumed.
    ///
    /// # Panics
    ///
    /// Panics if unread scripted steps, buffered inbound bytes, or pending
    /// virtual delay remain.
    pub fn assert_finished(&self) {
        assert!(
            self.is_finished(),
            "mock transport still has pending state: script={:?}, pending_read={:?}, pending_delay={:?}",
            self.script,
            self.pending_read,
            self.pending_delay
        );
    }

    const fn read_step(bytes: Vec<u8>) -> MockStep {
        MockStep::Read(bytes)
    }

    fn read_from_pending(&mut self, buffer: &mut [u8]) -> usize {
        let read_len = buffer.len().min(self.pending_read.len());

        for slot in buffer.iter_mut().take(read_len) {
            *slot = self
                .pending_read
                .pop_front()
                .expect("pending read length checked");
        }

        self.reads.extend_from_slice(&buffer[..read_len]);
        read_len
    }

    fn write_error(kind: io::ErrorKind, message: String) -> io::Error {
        io::Error::new(kind, message)
    }

    fn advance_delay(&mut self) -> io::Result<bool> {
        let Some(remaining) = self.pending_delay else {
            return Ok(false);
        };

        if remaining.is_zero() {
            self.pending_delay = None;
            return Ok(false);
        }

        if self.timeout.is_zero() {
            return Err(Self::write_error(
                io::ErrorKind::TimedOut,
                String::from("mock transport virtual delay cannot advance with a zero timeout"),
            ));
        }

        if remaining > self.timeout {
            self.pending_delay = Some(
                remaining
                    .checked_sub(self.timeout)
                    .expect("remaining delay exceeds timeout"),
            );
            return Err(Self::write_error(
                io::ErrorKind::TimedOut,
                format!(
                    "mock transport virtual delay exceeded timeout {:?}; {:?} still pending",
                    self.timeout,
                    self.pending_delay.expect("pending delay updated")
                ),
            ));
        }

        self.pending_delay = None;
        Ok(true)
    }

    fn unexpected_step_error(operation: &str, step: &MockStep) -> io::Error {
        io::Error::other(format!(
            "mock transport expected {operation}, found scripted step {step:?}"
        ))
    }
}

/// Abstracts the raw byte transport used by the recovery flow.
///
/// The recovery state machine, prompt parser, and XMODEM sender all operate on
/// raw UART bytes. This trait keeps that protocol logic independent from the
/// concrete serial backend so tests can use fakes while production code uses a
/// real device.
pub trait Transport {
    /// Reads available bytes into `buffer`.
    ///
    /// Returns the number of bytes read. Implementations should honor the
    /// transport's configured timeout.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while reading from the
    /// underlying transport.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;

    /// Writes the full `bytes` slice to the transport.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while writing to the
    /// underlying transport.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Flushes any buffered output.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while flushing pending output.
    fn flush(&mut self) -> io::Result<()>;

    /// Reconfigures the blocking timeout used by subsequent I/O operations.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while updating the transport
    /// timeout.
    fn set_timeout(&mut self, timeout: Duration) -> io::Result<()>;

    /// Reads at most one byte from the transport.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while reading the underlying
    /// transport.
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut buffer = [0_u8; 1];
        match self.read(&mut buffer)? {
            0 => Ok(None),
            _ => Ok(Some(buffer[0])),
        }
    }

    /// Writes a single byte and flushes it immediately.
    ///
    /// # Errors
    ///
    /// Returns any backend I/O failure produced while writing or flushing the
    /// underlying transport.
    fn write_byte(&mut self, byte: u8) -> io::Result<()> {
        self.write(&[byte])?;
        self.flush()
    }
}

impl Transport for SerialTransport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.port.read(buffer)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.port.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
    }

    fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.port
            .set_read_timeout(Some(timeout))
            .map_err(|error| map_serialport_error(self.path.as_str(), &error))?;
        self.port
            .set_write_timeout(Some(timeout))
            .map_err(|error| map_serialport_error(self.path.as_str(), &error))
    }
}

impl Transport for MockTransport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.pending_read.is_empty() {
                return Ok(self.read_from_pending(buffer));
            }

            if self.pending_delay.is_some() {
                self.advance_delay()?;
                continue;
            }

            match self.script.pop_front() {
                None => return Ok(0),
                Some(MockStep::Read(bytes)) => {
                    self.pending_read = bytes.into();
                }
                Some(MockStep::Delay(delay)) => {
                    self.pending_delay = Some(delay);
                }
                Some(MockStep::ReadError { kind, message }) => {
                    return Err(Self::write_error(kind, message));
                }
                Some(step) => return Err(Self::unexpected_step_error("read", &step)),
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.script.pop_front() {
            Some(MockStep::Write(expected)) => {
                if expected != bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "mock transport write mismatch: expected {expected:?}, got {bytes:?}"
                        ),
                    ));
                }
            }
            Some(MockStep::WriteError { kind, message }) => {
                return Err(Self::write_error(kind, message));
            }
            Some(step) => return Err(Self::unexpected_step_error("write", &step)),
            None => {}
        }

        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(step) = self.script.pop_front() {
            match step {
                MockStep::Flush => {}
                MockStep::FlushError { kind, message } => {
                    return Err(Self::write_error(kind, message));
                }
                _ => return Err(Self::unexpected_step_error("flush", &step)),
            }
        }

        self.flush_count += 1;
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        if let Some(step) = self.script.pop_front() {
            match step {
                MockStep::SetTimeout(expected) if expected == timeout => {}
                MockStep::SetTimeout(expected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "mock transport timeout mismatch: expected {expected:?}, got {timeout:?}"
                        ),
                    ));
                }
                _ => return Err(Self::unexpected_step_error("set_timeout", &step)),
            }
        }

        self.timeout = timeout;
        self.timeout_updates.push(timeout);
        Ok(())
    }
}

fn serial_port_builder(
    _path: &str,
    baud_rate: u32,
    timeout: Duration,
) -> serialport::SerialPortBuilder {
    serialport::SerialPort::builder()
        .baud_rate(baud_rate)
        .data_bits(DataBits::Eight)
        .flow_control(FlowControl::None)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .read_timeout(Some(timeout))
        .write_timeout(Some(timeout))
}

fn map_serialport_error(path: &str, error: &serialport::Error) -> io::Error {
    match error.kind() {
        serialport::ErrorKind::NoDevice => io::Error::new(
            io::ErrorKind::NotFound,
            format!("serial port {path} is unavailable: {}", error.description),
        ),
        serialport::ErrorKind::InvalidInput => io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid serial-port settings for {path}: {}",
                error.description
            ),
        ),
        serialport::ErrorKind::Unknown => {
            io::Error::other(format!("serial port {path} failed: {}", error.description))
        }
        serialport::ErrorKind::Io(kind) => io::Error::new(
            kind,
            format!("serial port {path} failed: {}", error.description),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BAUD_RATE, MockStep, MockTransport, Transport, map_serialport_error,
        serial_port_builder,
    };
    use serialport::{DataBits, FlowControl, Parity, StopBits};
    use std::io;
    use std::time::Duration;

    struct StubTransport {
        next_read: Vec<u8>,
        written: Vec<u8>,
        timeout: Duration,
        flush_count: usize,
    }

    impl StubTransport {
        fn new(next_read: Vec<u8>) -> Self {
            Self {
                next_read,
                written: Vec::new(),
                timeout: Duration::from_secs(1),
                flush_count: 0,
            }
        }
    }

    impl Transport for StubTransport {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read_len = buffer.len().min(self.next_read.len());
            buffer[..read_len].copy_from_slice(&self.next_read[..read_len]);
            self.next_read.drain(..read_len);
            Ok(read_len)
        }

        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_count += 1;
            Ok(())
        }

        fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.timeout = timeout;
            Ok(())
        }
    }

    #[test]
    fn read_byte_returns_none_when_transport_returns_zero_bytes() {
        let mut transport = StubTransport::new(Vec::new());

        assert_eq!(transport.read_byte().unwrap(), None);
    }

    #[test]
    fn read_byte_returns_the_next_available_byte() {
        let mut transport = StubTransport::new(vec![b'x', b'y']);

        assert_eq!(transport.read_byte().unwrap(), Some(b'x'));
        assert_eq!(transport.read_byte().unwrap(), Some(b'y'));
        assert_eq!(transport.read_byte().unwrap(), None);
    }

    #[test]
    fn write_byte_flushes_after_writing() {
        let mut transport = StubTransport::new(Vec::new());

        transport.write_byte(b'C').unwrap();

        assert_eq!(transport.written, vec![b'C']);
        assert_eq!(transport.flush_count, 1);
    }

    #[test]
    fn set_timeout_updates_the_transport_timeout() {
        let mut transport = StubTransport::new(Vec::new());
        let timeout = Duration::from_secs(5);

        transport.set_timeout(timeout).unwrap();

        assert_eq!(transport.timeout, timeout);
    }

    #[test]
    fn serial_port_builder_uses_documented_line_settings() {
        let timeout = Duration::from_millis(250);
        let builder = serial_port_builder("/dev/ttyUSB0", DEFAULT_BAUD_RATE, timeout);
        let expected = serialport::SerialPort::builder()
            .baud_rate(DEFAULT_BAUD_RATE)
            .data_bits(DataBits::Eight)
            .flow_control(FlowControl::None)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .read_timeout(Some(timeout))
            .write_timeout(Some(timeout));

        assert_eq!(builder, expected);
    }

    #[test]
    fn no_device_maps_to_not_found() {
        let error =
            serialport::Error::new(serialport::ErrorKind::NoDevice, "port is already in use");
        let io_error = map_serialport_error("/dev/ttyUSB0", &error);

        assert_eq!(io_error.kind(), io::ErrorKind::NotFound);
        assert!(io_error.to_string().contains("unavailable"));
    }

    #[test]
    fn invalid_input_maps_to_invalid_input() {
        let error = serialport::Error::new(serialport::ErrorKind::InvalidInput, "bad baud");
        let io_error = map_serialport_error("/dev/ttyUSB0", &error);

        assert_eq!(io_error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            io_error
                .to_string()
                .contains("invalid serial-port settings")
        );
    }

    #[test]
    fn io_error_preserves_the_underlying_io_kind() {
        let error = serialport::Error::new(
            serialport::ErrorKind::Io(io::ErrorKind::PermissionDenied),
            "permission denied",
        );
        let io_error = map_serialport_error("/dev/ttyUSB0", &error);

        assert_eq!(io_error.kind(), io::ErrorKind::PermissionDenied);
        assert!(io_error.to_string().contains("permission denied"));
    }

    #[test]
    fn mock_transport_replays_read_chunks_in_order() {
        let mut transport =
            MockTransport::from_rx_chunks([b"Press ".to_vec(), b"x\r\n".to_vec(), b"C".to_vec()]);
        let mut buffer = [0_u8; 16];

        assert_eq!(transport.read(&mut buffer).unwrap(), 6);
        assert_eq!(&buffer[..6], b"Press ");
        assert_eq!(transport.read(&mut buffer).unwrap(), 3);
        assert_eq!(&buffer[..3], b"x\r\n");
        assert_eq!(transport.read(&mut buffer).unwrap(), 1);
        assert_eq!(&buffer[..1], b"C");
        assert_eq!(transport.read(&mut buffer).unwrap(), 0);
        assert_eq!(transport.rx_log(), b"Press x\r\nC");
        transport.assert_finished();
    }

    #[test]
    fn mock_transport_splits_large_read_chunks_across_multiple_reads() {
        let mut transport = MockTransport::from_rx_chunks([b"fragmented".to_vec()]);
        let mut buffer = [0_u8; 4];

        assert_eq!(transport.read(&mut buffer).unwrap(), 4);
        assert_eq!(&buffer, b"frag");
        assert_eq!(transport.read(&mut buffer).unwrap(), 4);
        assert_eq!(&buffer, b"ment");
        assert_eq!(transport.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"ed");
        transport.assert_finished();
    }

    #[test]
    fn mock_transport_validates_expected_writes_and_flushes() {
        let mut transport = MockTransport::new([
            MockStep::Write(vec![b'x']),
            MockStep::Flush,
            MockStep::SetTimeout(Duration::from_secs(2)),
        ]);

        transport.write_byte(b'x').unwrap();
        transport.set_timeout(Duration::from_secs(2)).unwrap();

        assert_eq!(transport.writes(), &[vec![b'x']]);
        assert_eq!(transport.flush_count(), 1);
        assert_eq!(transport.timeout_updates(), &[Duration::from_secs(2)]);
        transport.assert_finished();
    }

    #[test]
    fn mock_transport_reports_write_mismatches() {
        let mut transport = MockTransport::new([MockStep::Write(vec![b'x'])]);
        let error = transport.write(b"y").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("write mismatch"));
    }

    #[test]
    fn mock_transport_can_inject_timeout_errors() {
        let mut transport = MockTransport::new([MockStep::ReadError {
            kind: io::ErrorKind::TimedOut,
            message: String::from("simulated timeout"),
        }]);
        let mut buffer = [0_u8; 8];
        let error = transport.read(&mut buffer).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("simulated timeout"));
    }

    #[test]
    fn mock_transport_uses_virtual_delay_without_sleeping() {
        let mut transport = MockTransport::new([
            MockStep::SetTimeout(Duration::from_millis(100)),
            MockStep::Delay(Duration::from_millis(250)),
            MockStep::SetTimeout(Duration::from_millis(75)),
            MockStep::Read(vec![b'C']),
        ]);
        let mut buffer = [0_u8; 8];

        transport.set_timeout(Duration::from_millis(100)).unwrap();
        let first = transport.read(&mut buffer).unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::TimedOut);

        transport.set_timeout(Duration::from_millis(75)).unwrap();
        let second = transport.read(&mut buffer).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::TimedOut);

        assert_eq!(transport.read(&mut buffer).unwrap(), 1);
        assert_eq!(&buffer[..1], b"C");
        assert_eq!(
            transport.timeout_updates(),
            &[Duration::from_millis(100), Duration::from_millis(75)]
        );
        transport.assert_finished();
    }

    #[test]
    fn mock_transport_can_inject_write_and_flush_errors() {
        let mut write_transport = MockTransport::new([MockStep::WriteError {
            kind: io::ErrorKind::BrokenPipe,
            message: String::from("uart disconnected"),
        }]);
        let write_error = write_transport.write(b"loadx\n").unwrap_err();
        assert_eq!(write_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(write_error.to_string().contains("uart disconnected"));

        let mut flush_transport = MockTransport::new([MockStep::FlushError {
            kind: io::ErrorKind::WouldBlock,
            message: String::from("flush stalled"),
        }]);
        let flush_error = flush_transport.flush().unwrap_err();
        assert_eq!(flush_error.kind(), io::ErrorKind::WouldBlock);
        assert!(flush_error.to_string().contains("flush stalled"));
    }

    #[test]
    fn mock_transport_reports_out_of_order_operations() {
        let mut transport = MockTransport::new([MockStep::Read(vec![b'C'])]);
        let error = transport.flush().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("expected flush"));
    }
}
