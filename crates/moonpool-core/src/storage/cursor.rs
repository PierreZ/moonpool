//! [`FileCursor`]: sequential reads and writes over a positioned file.

use std::io::{self, SeekFrom};
use std::ops::Deref;

use super::LocalStorageFile;

/// A read and write position over one already-open file.
///
/// [`StorageFile`](super::StorageFile) has no cursor: every transfer names
/// its offset. `FileCursor` keeps one offset and moves it after each
/// transfer, for code that reads or writes a file from start to end (a
/// configuration file, a snapshot, a test).
///
/// The cursor belongs to this value, not to the file. Two cursors over two
/// handles of one path do not share a position. The file is reachable through
/// `Deref`, so `cursor.sync_all()` and `cursor.read_at(..)` work as on the
/// file, and a positioned transfer does not move the cursor.
///
/// On a file with I/O constraints (a direct-I/O file) each transfer must
/// satisfy them, as for [`read_at`](LocalStorageFile::read_at). A short
/// transfer can leave the cursor at an offset that is not aligned, so use
/// [`BlockFile`](crate::BlockFile) for such a file.
///
/// ```
/// use moonpool_core::{FileCursor, LocalStorageFile};
///
/// async fn save<F: LocalStorageFile>(file: F, bytes: &[u8]) -> std::io::Result<()> {
///     let mut cursor = FileCursor::new(file);
///     cursor.write_all(bytes).await?;
///     cursor.sync_all().await
/// }
/// ```
#[derive(Debug)]
pub struct FileCursor<F> {
    file: F,
    position: u64,
}

impl<F> Deref for FileCursor<F> {
    type Target = F;

    fn deref(&self) -> &F {
        &self.file
    }
}

impl<F: LocalStorageFile> FileCursor<F> {
    /// Wrap `file` with the cursor at offset zero.
    pub fn new(file: F) -> Self {
        Self { file, position: 0 }
    }

    /// The offset of the next transfer.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// The wrapped file.
    pub fn get_ref(&self) -> &F {
        &self.file
    }

    /// Return the wrapped file.
    pub fn into_inner(self) -> F {
        self.file
    }

    /// Move the cursor and return its new offset.
    ///
    /// A position past the end of the file is valid: a write there extends
    /// the file.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`], with the cursor unchanged,
    /// when the target is before byte zero or does not fit in a `u64`.
    /// [`SeekFrom::End`] reads the size of the file and returns its error.
    pub async fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match to {
            SeekFrom::Start(offset) => (offset, 0),
            SeekFrom::End(delta) => (self.file.size().await?, delta),
            SeekFrom::Current(delta) => (self.position, delta),
        };
        let target = if delta >= 0 {
            base.checked_add(delta.unsigned_abs())
        } else {
            base.checked_sub(delta.unsigned_abs())
        };
        self.position = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )
        })?;
        Ok(self.position)
    }

    /// Read at the cursor and move it by the bytes read.
    ///
    /// The count can be less than `buf.len()`. Zero means end of file.
    ///
    /// # Errors
    ///
    /// Returns the error of the read. The cursor does not move.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read_at(self.position, buf).await?;
        self.position += read as u64;
        Ok(read)
    }

    /// Read until `buf` is full.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::UnexpectedEof`] when the file ends first, or
    /// the error of a read. The cursor stays after the bytes that were read.
    pub async fn read_exact(&mut self, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let read = self.read(buf).await?;
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf = &mut buf[read..];
        }
        Ok(())
    }

    /// Read from the cursor to the end of the file and append the bytes to
    /// `out`. Returns the number of bytes appended.
    ///
    /// # Errors
    ///
    /// Returns the error of a read. `out` keeps the bytes read before it.
    pub async fn read_to_end(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        const CHUNK: usize = 8 * 1024;
        let start = out.len();
        loop {
            let filled = out.len();
            out.resize(filled + CHUNK, 0);
            let result = self.read(&mut out[filled..]).await;
            out.truncate(filled + *result.as_ref().unwrap_or(&0));
            if result? == 0 {
                return Ok(out.len() - start);
            }
        }
    }

    /// Write at the cursor and move it by the bytes written.
    ///
    /// The count can be less than `buf.len()`.
    ///
    /// # Errors
    ///
    /// Returns the error of the write. The cursor does not move.
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file.write_at(self.position, buf).await?;
        self.position += written as u64;
        Ok(written)
    }

    /// Write all of `buf`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WriteZero`] when a write accepts no byte, or
    /// the error of a write. The cursor stays after the bytes that were
    /// written.
    pub async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let written = self.write(buf).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[written..];
        }
        Ok(())
    }
}
