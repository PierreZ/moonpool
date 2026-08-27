//! Production [`BlockDevice`] implementation over the local filesystem.
//!
//! ## Layout
//!
//! A device at `path` is a directory: a `manifest` file describing the region
//! layout, and one preallocated `region-<i>` file per region. Region sizes are
//! recovered from file lengths on [`open`](BlockDeviceProvider::open), so
//! [`BlockDevice::grow`] never rewrites the manifest.
//!
//! ## Atomic create
//!
//! [`create`](BlockDeviceProvider::create) builds the layout in a
//! `<path>.staging` directory. The first successful
//! [`persist`](BlockDevice::persist) syncs every file, renames the staging
//! directory to `path`, and fsyncs the parent directory — so after a crash
//! either the whole formatted device exists or nothing does. A leftover
//! staging directory from a crashed create is silently replaced by the next
//! `create`.
//!
//! ## Direct I/O and fail-stop
//!
//! Region files are opened with `O_DIRECT` where the platform and filesystem
//! support it (Linux; a filesystem that rejects the flag, such as tmpfs, falls
//! back to buffered I/O). I/O goes through sector-aligned bounce buffers via
//! positioned reads and writes on a blocking pool. Following the contract's
//! fail-stop clause, any [`persist`](BlockDevice::persist) failure poisons the
//! device: every subsequent operation returns [`BlockError::Io`], so callers
//! can never re-read stale clean-marked pages after a lying flush (Rebello et
//! al., ATC'20).

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use std::os::unix::fs::FileExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;

use crate::block::{
    BlockDevice, BlockDeviceProvider, BlockError, RegionId, RegionSpec, SECTOR_SIZE,
    validate_sector_range,
};

const MANIFEST_NAME: &str = "manifest";
const MANIFEST_HEADER: &str = "moonpool-blockdevice v1";

/// Production block-device provider over the local filesystem.
///
/// See the [module docs](self) for the on-disk layout, atomic-create, and
/// fail-stop behavior.
#[derive(Debug, Clone, Default)]
pub struct TokioBlockDeviceProvider;

impl TokioBlockDeviceProvider {
    /// Create a new provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl BlockDeviceProvider for TokioBlockDeviceProvider {
    type Device = TokioBlockDevice;

    async fn create(&self, path: &str, regions: &[RegionSpec]) -> Result<Self::Device, BlockError> {
        let path = PathBuf::from(path);
        let regions = regions.to_vec();
        run_blocking(move || create_device(&path, &regions)).await
    }

    async fn open(&self, path: &str) -> Result<Self::Device, BlockError> {
        let path = PathBuf::from(path);
        run_blocking(move || open_device(&path)).await
    }
}

/// Production block-device handle. Cloning shares the device.
#[derive(Debug, Clone)]
pub struct TokioBlockDevice {
    shared: Arc<DeviceShared>,
}

#[derive(Debug)]
struct DeviceShared {
    final_path: PathBuf,
    /// `Some(staging_dir)` until the first successful persist links the
    /// device; `None` afterwards.
    staging: Mutex<Option<PathBuf>>,
    regions: Vec<RegionFile>,
    /// Fail-stop marker: set on the first persist failure, poisons the device.
    failed: AtomicBool,
}

#[derive(Debug)]
struct RegionFile {
    file: fs::File,
    /// Visible region size in bytes (grow updates it before durability).
    size: AtomicU64,
    /// Byte ranges of writes currently in flight (overlap detection).
    in_flight: Mutex<Vec<Range<u64>>>,
}

fn create_device(path: &Path, regions: &[RegionSpec]) -> Result<TokioBlockDevice, BlockError> {
    if path.exists() {
        return Err(BlockError::AlreadyExists {
            path: path.display().to_string(),
        });
    }
    let staging = staging_path(path);
    if staging.exists() {
        // Leftover from a create that crashed before its first persist: the
        // atomic-create contract says it never existed.
        fs::remove_dir_all(&staging).map_err(|e| BlockError::io(format!("clear staging: {e}")))?;
    }
    fs::create_dir_all(&staging).map_err(|e| BlockError::io(format!("create staging: {e}")))?;

    let mut manifest = format!("{MANIFEST_HEADER}\n");
    let mut files = Vec::with_capacity(regions.len());
    for (index, spec) in regions.iter().enumerate() {
        if !spec.size.is_multiple_of(SECTOR_SIZE as u64) {
            return Err(BlockError::invalid_argument(format!(
                "region '{}' size {} is not a sector multiple",
                spec.name, spec.size
            )));
        }
        writeln!(manifest, "{} {}", spec.size, spec.name).expect("writing to a String cannot fail");
        let file_path = staging.join(region_file_name(index));
        let file = open_region_file(&file_path, true)?;
        file.set_len(spec.size)
            .map_err(|e| BlockError::io(format!("preallocate region {index}: {e}")))?;
        files.push(RegionFile {
            file,
            size: AtomicU64::new(spec.size),
            in_flight: Mutex::new(Vec::new()),
        });
    }
    let mut manifest_file = fs::File::create(staging.join(MANIFEST_NAME))
        .map_err(|e| BlockError::io(format!("create manifest: {e}")))?;
    manifest_file
        .write_all(manifest.as_bytes())
        .map_err(|e| BlockError::io(format!("write manifest: {e}")))?;

    Ok(TokioBlockDevice {
        shared: Arc::new(DeviceShared {
            final_path: path.to_path_buf(),
            staging: Mutex::new(Some(staging)),
            regions: files,
            failed: AtomicBool::new(false),
        }),
    })
}

fn open_device(path: &Path) -> Result<TokioBlockDevice, BlockError> {
    let manifest_path = path.join(MANIFEST_NAME);
    let mut manifest = String::new();
    match fs::File::open(&manifest_path) {
        Ok(mut file) => {
            file.read_to_string(&mut manifest)
                .map_err(|e| BlockError::io(format!("read manifest: {e}")))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BlockError::NotFound {
                path: path.display().to_string(),
            });
        }
        Err(e) => return Err(BlockError::io(format!("open manifest: {e}"))),
    }
    let mut lines = manifest.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(BlockError::io(format!(
            "corrupt manifest header in {}",
            manifest_path.display()
        )));
    }
    let region_count = lines.count();

    let mut files = Vec::with_capacity(region_count);
    for index in 0..region_count {
        let file_path = path.join(region_file_name(index));
        let file = open_region_file(&file_path, false)?;
        let size = file
            .metadata()
            .map_err(|e| BlockError::io(format!("stat region {index}: {e}")))?
            .len();
        files.push(RegionFile {
            file,
            size: AtomicU64::new(size),
            in_flight: Mutex::new(Vec::new()),
        });
    }
    Ok(TokioBlockDevice {
        shared: Arc::new(DeviceShared {
            final_path: path.to_path_buf(),
            staging: Mutex::new(None),
            regions: files,
            failed: AtomicBool::new(false),
        }),
    })
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".staging");
    PathBuf::from(name)
}

fn region_file_name(index: usize) -> String {
    format!("region-{index}")
}

/// Open a region file, attempting direct I/O first (Linux) and falling back
/// to buffered I/O where the flag is rejected (e.g. tmpfs).
fn open_region_file(path: &Path, create: bool) -> Result<fs::File, BlockError> {
    #[cfg(target_os = "linux")]
    {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(create);
        options.custom_flags(libc::O_DIRECT);
        if let Ok(file) = options.open(path) {
            return Ok(file);
        }
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .open(path)
        .map_err(|e| BlockError::io(format!("open {}: {e}", path.display())))
}

/// A sector-aligned zeroed byte buffer, built from safe code by over-allocating
/// and slicing at the alignment boundary (direct I/O requires aligned memory).
struct AlignedBuf {
    backing: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    fn zeroed(len: usize) -> Self {
        let backing = vec![0u8; len + SECTOR_SIZE];
        let offset = backing.as_ptr().align_offset(SECTOR_SIZE);
        Self {
            backing,
            offset,
            len,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.backing[self.offset..self.offset + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.backing[self.offset..self.offset + self.len]
    }
}

impl DeviceShared {
    fn region(&self, region: RegionId) -> Result<&RegionFile, BlockError> {
        let index = usize::try_from(region.0).expect("region index fits in usize");
        self.regions
            .get(index)
            .ok_or_else(|| BlockError::invalid_argument(format!("unknown region {region:?}")))
    }

    fn check_not_failed(&self) -> Result<(), BlockError> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(BlockError::io(
                "block device is fail-stopped after a persist error",
            ));
        }
        Ok(())
    }

    /// Sync every region file; on the first persist, also sync the manifest,
    /// rename the staging directory into place, and fsync the parent.
    fn persist_blocking(&self) -> Result<(), BlockError> {
        for (index, region) in self.regions.iter().enumerate() {
            region
                .file
                .sync_all()
                .map_err(|e| BlockError::io(format!("sync region {index}: {e}")))?;
        }
        let mut staging = self
            .staging
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        if let Some(staging_dir) = staging.as_ref() {
            let manifest = fs::File::open(staging_dir.join(MANIFEST_NAME))
                .map_err(|e| BlockError::io(format!("open manifest for sync: {e}")))?;
            manifest
                .sync_all()
                .map_err(|e| BlockError::io(format!("sync manifest: {e}")))?;
            fs::rename(staging_dir, &self.final_path)
                .map_err(|e| BlockError::io(format!("link device: {e}")))?;
            if let Some(parent) = self.final_path.parent() {
                let dir = fs::File::open(parent)
                    .map_err(|e| BlockError::io(format!("open parent dir: {e}")))?;
                dir.sync_all()
                    .map_err(|e| BlockError::io(format!("sync parent dir: {e}")))?;
            }
            *staging = None;
        }
        Ok(())
    }
}

impl RegionFile {
    /// Reserve a write range, asserting the contract's no-concurrent-overlap
    /// clause.
    fn reserve(&self, range: Range<u64>) {
        let mut in_flight = self
            .in_flight
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        assert!(
            !in_flight
                .iter()
                .any(|r| r.start < range.end && range.start < r.end),
            "concurrent overlapping writes to one region are a caller bug (range {range:?})"
        );
        in_flight.push(range);
    }

    fn release(&self, range: &Range<u64>) {
        let mut in_flight = self
            .in_flight
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        if let Some(position) = in_flight.iter().position(|r| r == range) {
            in_flight.remove(position);
        }
    }
}

async fn run_blocking<T, F>(work: F) -> Result<T, BlockError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, BlockError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| BlockError::io(format!("blocking task failed: {e}")))?
}

impl BlockDevice for TokioBlockDevice {
    async fn read(&self, region: RegionId, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.shared.check_not_failed()?;
        let target = self.shared.region(region)?;
        validate_sector_range(offset, buf.len(), target.size.load(Ordering::SeqCst))?;
        if buf.is_empty() {
            return Ok(());
        }
        let shared = Arc::clone(&self.shared);
        let len = buf.len();
        let bytes = run_blocking(move || {
            let target = shared.region(region)?;
            let mut bounce = AlignedBuf::zeroed(len);
            target
                .file
                .read_exact_at(bounce.as_mut_slice(), offset)
                .map_err(|e| BlockError::io(format!("read region {}: {e}", region.0)))?;
            Ok(bounce)
        })
        .await?;
        buf.copy_from_slice(bytes.as_slice());
        Ok(())
    }

    async fn write(&self, region: RegionId, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.shared.check_not_failed()?;
        let target = self.shared.region(region)?;
        validate_sector_range(offset, buf.len(), target.size.load(Ordering::SeqCst))?;
        if buf.is_empty() {
            return Ok(());
        }
        let range = offset..offset + buf.len() as u64;
        target.reserve(range.clone());
        let mut bounce = AlignedBuf::zeroed(buf.len());
        bounce.as_mut_slice().copy_from_slice(buf);
        let shared = Arc::clone(&self.shared);
        let result = run_blocking(move || {
            let target = shared.region(region)?;
            target
                .file
                .write_all_at(bounce.as_slice(), offset)
                .map_err(|e| BlockError::io(format!("write region {}: {e}", region.0)))
        })
        .await;
        self.shared.region(region)?.release(&range);
        result
    }

    async fn persist(&self) -> Result<(), BlockError> {
        self.shared.check_not_failed()?;
        let shared = Arc::clone(&self.shared);
        let result = run_blocking(move || shared.persist_blocking()).await;
        if result.is_err() {
            // Fail-stop: a failed barrier means the kernel may have dropped
            // dirty pages while marking them clean — nothing read after this
            // point can be trusted, so the device refuses further work.
            self.shared.failed.store(true, Ordering::SeqCst);
        }
        result
    }

    async fn grow(&self, region: RegionId, new_size: u64) -> Result<(), BlockError> {
        self.shared.check_not_failed()?;
        let target = self.shared.region(region)?;
        if !new_size.is_multiple_of(SECTOR_SIZE as u64) {
            return Err(BlockError::invalid_argument(format!(
                "grow size {new_size} is not a sector multiple"
            )));
        }
        let current = target.size.load(Ordering::SeqCst);
        if new_size < current {
            return Err(BlockError::invalid_argument(format!(
                "grow is grow-only: {new_size} < current size {current}"
            )));
        }
        let shared = Arc::clone(&self.shared);
        run_blocking(move || {
            let target = shared.region(region)?;
            target
                .file
                .set_len(new_size)
                .map_err(|e| BlockError::io(format!("grow region {}: {e}", region.0)))?;
            target.size.store(new_size, Ordering::SeqCst);
            Ok(())
        })
        .await
    }

    fn region_size(&self, region: RegionId) -> u64 {
        let index = usize::try_from(region.0).expect("region index fits in usize");
        self.shared.regions[index].size.load(Ordering::SeqCst)
    }

    fn region_count(&self) -> u32 {
        u32::try_from(self.shared.regions.len()).expect("region count fits in u32")
    }
}
