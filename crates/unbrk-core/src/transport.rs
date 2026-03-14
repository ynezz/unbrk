//! Blocking serial transport abstractions.

use serialport::{DataBits, FlowControl, Parity, StopBits};
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
    use super::{DEFAULT_BAUD_RATE, Transport, map_serialport_error, serial_port_builder};
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
}
