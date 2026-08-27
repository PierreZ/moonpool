//! Region-based block-device contract with a barrier-bounded crash model.
//!
//! [`BlockDevice`] is the narrow storage surface production engines actually
//! consume (a WAL, an LSM, a B-tree pager): sector-aligned reads and writes
//! inside named regions, a durability barrier, and grow-only resize. It sits
//! deliberately *below* [`StorageProvider`](crate::StorageProvider)'s
//! POSIX-flavored stream API: no seek, no append mode, no auto-extend — and in
//! exchange it guarantees exactly what an engine needs (atomicity unit,
//! alignment, reorder window across barriers).
//!
//! The contract clauses documented on [`BlockDevice`] ARE the feature: the
//! simulation implementation in `moonpool-sim` exists to produce every state
//! the clauses permit (torn multi-sector writes, reordering across an open
//! barrier window, lost unsynced sectors), so recovery code that must survive
//! those states can actually be driven red.

use thiserror::Error;

/// Size of one device sector in bytes.
///
/// All offsets and lengths passed to [`BlockDevice::read`],
/// [`BlockDevice::write`], and [`BlockDevice::grow`] must be multiples of this
/// value. One sector is also the write atomicity unit — see the contract on
/// [`BlockDevice`].
pub const SECTOR_SIZE: usize = 4096;

/// Identifier of one region inside a block device.
///
/// Regions are identified by their index in the [`RegionSpec`] slice passed to
/// [`BlockDeviceProvider::create`]: the i-th spec is `RegionId(i)`. The
/// mapping is part of the device layout and is preserved by
/// [`BlockDeviceProvider::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u32);

/// Layout description of one region at device-creation time.
#[derive(Debug, Clone, Copy)]
pub struct RegionSpec {
    /// Human-readable region name (e.g. `"wal"`, `"superblock"`). Used for
    /// diagnostics; region identity at runtime is the [`RegionId`] index.
    pub name: &'static str,
    /// Initial region size in bytes. Must be a multiple of [`SECTOR_SIZE`].
    pub size: u64,
}

/// Errors returned by [`BlockDevice`] and [`BlockDeviceProvider`] operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BlockError {
    /// An I/O error (EIO class). This is an *operating condition* — a device
    /// reporting an error on an operation — and is distinct from a read that
    /// succeeds but returns corrupt bytes. Callers must handle both.
    #[error("block device I/O error: {message}")]
    Io {
        /// Description of the failure.
        message: String,
    },

    /// No device exists at the given path.
    #[error("block device not found: {path}")]
    NotFound {
        /// The path that was opened.
        path: String,
    },

    /// A device already exists at the given path.
    #[error("block device already exists: {path}")]
    AlreadyExists {
        /// The path that was created.
        path: String,
    },

    /// The caller violated the API contract (misaligned offset or length,
    /// out-of-bounds access, unknown region, shrinking `grow`, ...).
    #[error("invalid block device argument: {message}")]
    InvalidArgument {
        /// Description of the violation.
        message: String,
    },
}

impl BlockError {
    /// Build an [`BlockError::InvalidArgument`] from anything displayable.
    #[must_use]
    pub fn invalid_argument(message: impl std::fmt::Display) -> Self {
        Self::InvalidArgument {
            message: message.to_string(),
        }
    }

    /// Build an [`BlockError::Io`] from anything displayable.
    #[must_use]
    pub fn io(message: impl std::fmt::Display) -> Self {
        Self::Io {
            message: message.to_string(),
        }
    }
}

/// A region-addressed block device with an explicit durability barrier.
///
/// # Contract
///
/// These clauses are the API. Implementations must uphold them and consumers
/// may rely on nothing stronger:
///
/// - **Atomicity unit is one sector, nothing larger.** A crash may
///   independently leave each sector of a multi-sector write old, new, or
///   unreadable. (`NVMe` `AWUPF` formalizes per-sector atomicity; simulation
///   configurations may additionally model pre-`AWUPF` *shorn* sub-sector tears
///   as an opt-in, which weakens this clause.)
/// - **Writes between two [`persist`](Self::persist) calls may reach disk in
///   any order.** `persist()` orders everything before it against everything
///   after it. There is no other ordering guarantee.
/// - **Completion of [`write`](Self::write) implies visibility, not
///   durability.** A completed write is observable by subsequent reads, but
///   only a completed `persist()` makes it survive a crash.
/// - **Never-written sectors read unspecified bytes** — zeros, stale data, or
///   garbage. Consumers must never infer written-ness from content (SATA `RZAT`
///   and `NVMe` `DLFEAT` make deterministic zeros a *real* case, so
///   content-sniffing bugs survive garbage-only testing).
/// - **[`BlockError::Io`] (EIO) is an operating condition**, distinct from a
///   successful read of corrupt bytes.
/// - **A read of a faulted sector returns the same corrupt bytes on retry.**
///   Corruption is deterministic; retries must not heal.
/// - **Concurrent overlapping writes to one region are a caller bug** and are
///   asserted against by implementations.
/// - **Production implementations use O_DIRECT-style I/O** (no shared page
///   cache), so that fail-stop after a `persist()` error cannot re-read stale
///   clean-marked pages (Rebello et al., ATC'20). After a failed `persist()`,
///   callers must treat the device as failed rather than retry-and-trust.
pub trait BlockDevice: Send + Sync + 'static {
    /// Read sectors from a region into `buf`.
    ///
    /// `offset` and `buf.len()` must be multiples of [`SECTOR_SIZE`], and
    /// `offset + buf.len()` must be within the region.
    fn read(
        &self,
        region: RegionId,
        offset: u64,
        buf: &mut [u8],
    ) -> impl std::future::Future<Output = Result<(), BlockError>> + Send;

    /// Write sectors to a region.
    ///
    /// `offset` and `buf.len()` must be multiples of [`SECTOR_SIZE`], and
    /// `offset + buf.len()` must be within the region. Atomicity unit is one
    /// sector, nothing larger. Completion implies visibility, not durability.
    fn write(
        &self,
        region: RegionId,
        offset: u64,
        buf: &[u8],
    ) -> impl std::future::Future<Output = Result<(), BlockError>> + Send;

    /// Durability barrier: on `Ok`, every previously *completed* write (and
    /// [`grow`](Self::grow)) on this device is durable.
    fn persist(&self) -> impl std::future::Future<Output = Result<(), BlockError>> + Send;

    /// Grow-only resize of a region to `new_size` bytes (a multiple of
    /// [`SECTOR_SIZE`], `>=` the current size). The new size is visible
    /// immediately but durable only after the next [`persist`](Self::persist);
    /// a crash before that may revert the region to its last durable size.
    fn grow(
        &self,
        region: RegionId,
        new_size: u64,
    ) -> impl std::future::Future<Output = Result<(), BlockError>> + Send;

    /// Current (visible) size of a region in bytes.
    ///
    /// # Panics
    ///
    /// May panic if `region` is not a region of this device.
    fn region_size(&self, region: RegionId) -> u64;

    /// Number of regions in this device's layout.
    fn region_count(&self) -> u32;
}

/// Factory for [`BlockDevice`] instances.
pub trait BlockDeviceProvider: Clone + Send + Sync + 'static {
    /// The device type produced by this provider.
    type Device: BlockDevice;

    /// Atomically create a device at `path` with the given region layout.
    ///
    /// The device is invisible to [`open`](Self::open) until its first
    /// successful [`persist`](BlockDevice::persist): after a crash, either the
    /// whole formatted layout exists or nothing does.
    fn create(
        &self,
        path: &str,
        regions: &[RegionSpec],
    ) -> impl std::future::Future<Output = Result<Self::Device, BlockError>> + Send;

    /// Open an existing device at `path`.
    fn open(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Self::Device, BlockError>> + Send;
}

/// Validate that `offset`/`len` describe a sector-aligned range fully inside a
/// region of `region_size` bytes.
///
/// Shared by implementations so alignment/bounds errors are uniform.
///
/// # Errors
///
/// Returns [`BlockError::InvalidArgument`] when the range is misaligned or out
/// of bounds.
pub fn validate_sector_range(offset: u64, len: usize, region_size: u64) -> Result<(), BlockError> {
    let sector = SECTOR_SIZE as u64;
    if !offset.is_multiple_of(sector) {
        return Err(BlockError::invalid_argument(format!(
            "offset {offset} is not sector-aligned (sector size {SECTOR_SIZE})"
        )));
    }
    if !len.is_multiple_of(SECTOR_SIZE) {
        return Err(BlockError::invalid_argument(format!(
            "length {len} is not a sector multiple (sector size {SECTOR_SIZE})"
        )));
    }
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| BlockError::invalid_argument("offset + length overflows u64"))?;
    if end > region_size {
        return Err(BlockError::invalid_argument(format!(
            "range [{offset}, {end}) exceeds region size {region_size}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_range_validation() {
        let region = 16 * SECTOR_SIZE as u64;
        assert!(validate_sector_range(0, SECTOR_SIZE, region).is_ok());
        assert!(validate_sector_range(SECTOR_SIZE as u64, 4 * SECTOR_SIZE, region).is_ok());
        assert!(validate_sector_range(0, 16 * SECTOR_SIZE, region).is_ok());

        // Misaligned offset and length.
        assert!(validate_sector_range(1, SECTOR_SIZE, region).is_err());
        assert!(validate_sector_range(0, SECTOR_SIZE - 1, region).is_err());
        // Out of bounds and overflow.
        assert!(validate_sector_range(region, SECTOR_SIZE, region).is_err());
        assert!(validate_sector_range(u64::MAX - 4095, SECTOR_SIZE, region).is_err());
    }
}
