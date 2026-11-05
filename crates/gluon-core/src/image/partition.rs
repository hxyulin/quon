//! Partition formatting: hand a byte-range view of the disk to hadris-fat.
//!
//! [`PartitionWindow`] wraps a `File` and translates all I/O to live
//! within `[base, base + len)`. hadris-fat's `format()` consumes a
//! `Read + Write + Seek` (with the `std` feature, that's `std::io::*`),
//! so once the window is constructed we hand it off and let hadris
//! treat the partition as if it were an isolated disk.
//!
//! ## Why a window instead of a separate file?
//!
//! Three options were considered:
//!
//! 1. **Mount the partition's bytes as a separate temp file**, format
//!    it, then memcpy the bytes back into the disk image. Robust but
//!    doubles the I/O and requires twice the disk space.
//! 2. **Slice the disk file's mmap** and feed that to hadris. Avoids
//!    extra I/O, but needs hadris to accept `&mut [u8]` (it doesn't —
//!    it wants `Read + Write + Seek`) and complicates lifetimes.
//! 3. **Wrap a `File` handle** that translates positions on the fly.
//!    This is the cheapest option that fits hadris's API, and it
//!    composes naturally with the `RawWrite` pass that runs before it
//!    (each pass owns a fresh handle to the same file — no cross-pass
//!    state to keep consistent).
//!
//! We picked (3).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// A view of a `File` restricted to `[base, base + len)`. All I/O
/// positions are reported and seek relative to `base`. Writes that
/// would extend past `len` short-write — hadris-fat treats this as a
/// disk-full error, which is the correct semantics for a partition
/// that's too small for its contents.
pub struct PartitionWindow {
    file: File,
    base: u64,
    len: u64,
    /// Logical position within the window (0..=len). Tracked
    /// separately from the underlying file's seek position because
    /// `Read::read` and `Write::write` have to seek-then-read/write
    /// per call — sharing the file's actual position would race with
    /// any other handle on the same fd.
    pos: u64,
}

impl PartitionWindow {
    /// `start_byte` and `len` are byte offsets/sizes (not LBAs).
    /// Caller is responsible for ensuring `start_byte + len <= file.len()`.
    pub fn new(file: File, start_byte: u64, len: u64) -> Self {
        Self {
            file,
            base: start_byte,
            len,
            pos: 0,
        }
    }
}

impl Read for PartitionWindow {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let avail = (self.len - self.pos).min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.file.read(&mut buf[..avail])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for PartitionWindow {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            // Out of partition space. WriteZero is the std::io
            // convention for "couldn't write any of the requested bytes
            // because we hit a hard cap" — the partition is full.
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partition window full",
            ));
        }
        let avail = (self.len - self.pos).min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.file.write(&buf[..avail])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for PartitionWindow {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Compute in i128 to avoid overflow when `Current(-n)` /
        // `End(-n)` underflows a u64. Negative results below are
        // explicitly rejected.
        let new = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
        };
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before partition start",
            ));
        }
        // Permit seeking past `len` — std::fs::File allows this on
        // every platform we target, and hadris-fat occasionally seeks
        // past the end before backing off. Reads from such a position
        // will return 0; writes will short-write per the cap above.
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    /// Sanity: write at logical pos 100 + read it back; verify the
    /// underlying file got the bytes at base+100 (not pos 100).
    #[test]
    fn partition_window_translates_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        const BASE: u64 = 1024;
        const LEN: u64 = 1024;

        // Open via the window, seek + write.
        {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut w = PartitionWindow::new(f, BASE, LEN);
            w.seek(SeekFrom::Start(100)).unwrap();
            w.write_all(b"hello").unwrap();
            w.flush().unwrap();
        }

        // Read back via a raw file handle.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[(BASE as usize + 100)..(BASE as usize + 105)],
            b"hello"
        );
        assert!(
            bytes[..(BASE as usize)].iter().all(|&b| b == 0),
            "bytes before window must not be touched"
        );
    }

    /// Writes past the window length should short-write rather than
    /// silently corrupting the next partition.
    #[test]
    fn partition_window_short_writes_at_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut w = PartitionWindow::new(f, 0, 8);
        let written = w.write(&[0xAAu8; 16]).unwrap();
        assert_eq!(written, 8, "should short-write to fit the window");

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes[..8].iter().all(|&b| b == 0xAA),
            "first 8 bytes get the 0xAA"
        );
        assert_eq!(bytes[8], 0, "byte just past the window must not be touched");
    }

    /// Reads beyond the window return 0 (EOF), not garbage from the
    /// next partition.
    #[test]
    fn partition_window_reads_eof_at_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk");
        // Pre-fill so any escape-the-window read would return non-zero.
        std::fs::write(&path, vec![0xFFu8; 4096]).unwrap();

        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut w = PartitionWindow::new(f, 100, 16);
        w.seek(SeekFrom::Start(16)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(w.read(&mut buf).unwrap(), 0, "EOF at window end");
    }
}
