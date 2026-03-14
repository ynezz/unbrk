//! Blocking serial transport abstractions.

use std::io;
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::Transport;
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
}
