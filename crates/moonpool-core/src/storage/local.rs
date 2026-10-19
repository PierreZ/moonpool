//! Local (non-`Send`) storage traits: [`LocalStorageProvider`] and
//! [`LocalStorageFile`].
//!
//! A thread-per-core runtime (compio, monoio) pins every task, file handle and
//! in-flight operation to one thread. Its files hold an `Rc` to their
//! descriptor and its futures hold the thread's proactor, so neither is `Send`
//! and such a backend cannot implement [`StorageProvider`] or [`StorageFile`].
//! The local traits are the same contract with every `Send` and `Sync` bound
//! removed: from the trait, from its associated types and from its returned
//! futures.
//!
//! ## Relationship to the `Send` traits
//!
//! The local traits are the base. Every [`StorageProvider`] is a
//! [`LocalStorageProvider`] and every [`StorageFile`] is a
//! [`LocalStorageFile`], through blanket impls that forward each method, so
//! the Tokio and simulated backends implement the local traits for free. A
//! thread-per-core backend implements only the local traits.
//!
//! Code written against the local traits runs on every backend. When the
//! backend is `Send` (Tokio, the simulator), the futures it returns are
//! `Send` too: the auto traits of the concrete future leak through the
//! generic code at the concrete call site, so that code can still be spawned
//! on a work-stealing runtime.
//!
//! ## Method-name ambiguity
//!
//! A type that implements [`StorageFile`] also implements
//! [`LocalStorageFile`], with the same method names. With both traits in
//! scope, `file.read_at(..)` on a concrete type is ambiguous. Import only one
//! of the two traits in a module, or call through the trait
//! (`LocalStorageFile::read_at(&file, ..)`). Generic code bounded by one trait
//! is not affected.

use std::future::Future;
use std::io;

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use super::{AlignedBuf, IoConstraints, OpenOptions, StorageFile, StorageProvider};

/// Local (non-`Send`) form of [`StorageProvider`]: the filesystem namespace.
///
/// The same contract as [`StorageProvider`], method by method, without any
/// `Send` or `Sync` bound. Every [`StorageProvider`] implements it through a
/// blanket impl; a thread-per-core backend implements it directly.
///
/// Code generic over this trait runs on every backend, and when the backend
/// is `Send` (Tokio, the simulator) the futures it builds are `Send` too:
/// the auto traits of the concrete future leak through the generic code.
///
/// A concrete [`StorageProvider`] has both traits' methods under the same
/// names. With both traits in scope, `provider.open(..)` is ambiguous: import
/// one trait per module, or call `LocalStorageProvider::open(&provider, ..)`.
pub trait LocalStorageProvider: Clone + 'static {
    /// The file type for this provider.
    type File: LocalStorageFile + 'static;

    /// Open a file with the given options. See [`StorageProvider::open`].
    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl Future<Output = io::Result<Self::File>>;

    /// Check if a file or directory exists at the given path. See
    /// [`StorageProvider::exists`].
    fn exists(&self, path: &str) -> impl Future<Output = io::Result<bool>>;

    /// Delete a file at the given path. See [`StorageProvider::delete`].
    fn delete(&self, path: &str) -> impl Future<Output = io::Result<()>>;

    /// Rename a file from one path to another. See
    /// [`StorageProvider::rename`].
    fn rename(&self, from: &str, to: &str) -> impl Future<Output = io::Result<()>>;

    /// Create `path` and any missing parent directories. The new names are
    /// visible, not durable. See [`StorageProvider::create_dir_all`].
    fn create_dir_all(&self, path: &str) -> impl Future<Output = io::Result<()>>;

    /// List the names directly under the directory `path`, sorted. See
    /// [`StorageProvider::list_dir`].
    fn list_dir(&self, path: &str) -> impl Future<Output = io::Result<Vec<String>>>;

    /// Make the directory entries under `path` durable. See
    /// [`StorageProvider::sync_dir`].
    fn sync_dir(&self, path: &str) -> impl Future<Output = io::Result<()>>;
}

/// Local (non-`Send`) form of [`StorageFile`]: one already-open file.
///
/// The same contract as [`StorageFile`] (durability, stream I/O versus
/// alignment, borrowed versus owned buffers), method by method, without any
/// `Send` or `Sync` bound. Every [`StorageFile`] implements it through a
/// blanket impl, which forwards every method, the defaults included; a
/// thread-per-core backend implements it directly.
///
/// Code generic over this trait runs on every backend, and when the backend
/// is `Send` (Tokio, the simulator) the futures it builds are `Send` too:
/// the auto traits of the concrete future leak through the generic code.
///
/// A concrete [`StorageFile`] has both traits' methods under the same names.
/// With both traits in scope, `file.read_at(..)` is ambiguous: import one
/// trait per module, or call `LocalStorageFile::read_at(&file, ..)`.
pub trait LocalStorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'static {
    /// Flush this file's data and metadata to the device. See
    /// [`StorageFile::sync_all`].
    fn sync_all(&self) -> impl Future<Output = io::Result<()>>;

    /// Flush this file's data to the device; metadata may lag. See
    /// [`StorageFile::sync_data`].
    fn sync_data(&self) -> impl Future<Output = io::Result<()>>;

    /// The alignment this file requires of offsets, lengths and buffer
    /// addresses. See [`StorageFile::constraints`].
    fn constraints(&self) -> IoConstraints;

    /// Whether this file is actually doing direct (uncached) I/O. See
    /// [`StorageFile::is_direct_io`].
    fn is_direct_io(&self) -> bool;

    /// Read into `buf` starting at `offset`; possibly short, `0` at end of
    /// file. See [`StorageFile::read_at`].
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>>;

    /// Write `buf` starting at `offset`; possibly short, visible and not
    /// durable. See [`StorageFile::write_at`].
    fn write_at(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>>;

    /// Write `buf` starting at `offset` and make the written bytes (and only
    /// those) durable before returning. See [`StorageFile::write_at_durable`].
    ///
    /// ## Default
    ///
    /// [`write_at`](Self::write_at) then [`sync_data`](Self::sync_data).
    fn write_at_durable(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        async move {
            let written = LocalStorageFile::write_at(self, offset, buf).await?;
            LocalStorageFile::sync_data(self).await?;
            Ok(written)
        }
    }

    /// [`read_at`](Self::read_at) into an owned, aligned buffer, handed back
    /// with the count. On error the buffer is dropped. See
    /// [`StorageFile::read_at_aligned`].
    ///
    /// ## Default
    ///
    /// [`read_at`](Self::read_at) on the buffer's bytes.
    fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        async move {
            let mut buf = buf;
            let read = LocalStorageFile::read_at(self, offset, buf.as_mut_slice()).await?;
            Ok((read, buf))
        }
    }

    /// [`write_at`](Self::write_at) from an owned, aligned buffer, handed
    /// back with the count. On error the buffer is dropped. See
    /// [`StorageFile::write_at_aligned`].
    ///
    /// ## Default
    ///
    /// [`write_at`](Self::write_at) on the buffer's bytes.
    fn write_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        async move {
            let written = LocalStorageFile::write_at(self, offset, buf.as_slice()).await?;
            Ok((written, buf))
        }
    }

    /// [`write_at_durable`](Self::write_at_durable) from an owned, aligned
    /// buffer, handed back with the count. On error the buffer is dropped and
    /// the range's durability is unknown. See
    /// [`StorageFile::write_at_durable_aligned`].
    ///
    /// ## Default
    ///
    /// [`write_at_durable`](Self::write_at_durable) on the buffer's bytes.
    fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        async move {
            let written = LocalStorageFile::write_at_durable(self, offset, buf.as_slice()).await?;
            Ok((written, buf))
        }
    }

    /// Get the current size of the file in bytes. See [`StorageFile::size`].
    fn size(&self) -> impl Future<Output = io::Result<u64>>;

    /// Set the length of the file, growing or shrinking it. See
    /// [`StorageFile::set_len`].
    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>>;
}

impl<P: StorageProvider> LocalStorageProvider for P {
    type File = P::File;

    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl Future<Output = io::Result<Self::File>> {
        StorageProvider::open(self, path, options)
    }

    fn exists(&self, path: &str) -> impl Future<Output = io::Result<bool>> {
        StorageProvider::exists(self, path)
    }

    fn delete(&self, path: &str) -> impl Future<Output = io::Result<()>> {
        StorageProvider::delete(self, path)
    }

    fn rename(&self, from: &str, to: &str) -> impl Future<Output = io::Result<()>> {
        StorageProvider::rename(self, from, to)
    }

    fn create_dir_all(&self, path: &str) -> impl Future<Output = io::Result<()>> {
        StorageProvider::create_dir_all(self, path)
    }

    fn list_dir(&self, path: &str) -> impl Future<Output = io::Result<Vec<String>>> {
        StorageProvider::list_dir(self, path)
    }

    fn sync_dir(&self, path: &str) -> impl Future<Output = io::Result<()>> {
        StorageProvider::sync_dir(self, path)
    }
}

/// Forwards every method, the defaults included, so a backend's own override
/// of a default (the Tokio backend's `RWF_DSYNC` write, its zero-copy owned
/// transfers) applies through the local trait too.
impl<F: StorageFile> LocalStorageFile for F {
    fn sync_all(&self) -> impl Future<Output = io::Result<()>> {
        StorageFile::sync_all(self)
    }

    fn sync_data(&self) -> impl Future<Output = io::Result<()>> {
        StorageFile::sync_data(self)
    }

    fn constraints(&self) -> IoConstraints {
        StorageFile::constraints(self)
    }

    fn is_direct_io(&self) -> bool {
        StorageFile::is_direct_io(self)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> {
        StorageFile::read_at(self, offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        StorageFile::write_at(self, offset, buf)
    }

    fn write_at_durable(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        StorageFile::write_at_durable(self, offset, buf)
    }

    fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        StorageFile::read_at_aligned(self, offset, buf)
    }

    fn write_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        StorageFile::write_at_aligned(self, offset, buf)
    }

    fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> {
        StorageFile::write_at_durable_aligned(self, offset, buf)
    }

    fn size(&self) -> impl Future<Output = io::Result<u64>> {
        StorageFile::size(self)
    }

    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>> {
        StorageFile::set_len(self, size)
    }
}
