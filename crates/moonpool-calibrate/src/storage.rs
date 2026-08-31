//! Storage latency measured against the real filesystem.
//!
//! Everything here is `std::fs` / `std::io` plus [`Instant`]. No moonpool
//! storage provider, no simulated time, no simulated randomness: the point of
//! the exercise is to learn what the host actually does, so that a simulation
//! can be told to imitate it.
//!
//! The methodology is intentionally simple and makes no attempt to be `fio`:
//!
//! - A scratch file is created once and filled with a fixed pattern. File
//!   creation and preallocation are *not* timed.
//! - `read`, `write` and `sync` are each warmed up before the recorded samples
//!   start, so first-touch page faults and metadata allocation do not land in
//!   the histogram.
//! - Each read/write targets a block chosen by a small deterministic PRNG, so
//!   the access pattern is not purely sequential but is reproducible.
//! - `sync` is timed on its own: a block is written *untimed* to leave dirty
//!   data behind, then only [`File::sync_all`] is measured.
//! - Read bytes are folded into a checksum handed to [`std::hint::black_box`],
//!   so the optimiser cannot delete the work being measured.
//! - The scratch file is removed by a drop guard, including on the error path.
//!
//! Page cache effects are *not* defeated. Reads that hit the cache are part of
//! what a real process experiences, and `O_DIRECT` is out of scope for a
//! calibration utility.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::stats::Latencies;

/// Size of a single measured I/O, in bytes.
pub const BLOCK_SIZE: usize = 4096;

/// Number of blocks in the scratch file (4 MiB total).
pub const BLOCK_COUNT: u64 = 1024;

/// Percentile summaries for the three storage operation classes moonpool models.
#[derive(Debug)]
pub struct StorageMeasurements {
    /// Latency of a single `BLOCK_SIZE` read.
    pub read: Latencies,
    /// Latency of a single `BLOCK_SIZE` write.
    pub write: Latencies,
    /// Latency of `sync_all` with one dirty block outstanding.
    pub sync: Latencies,
}

/// Removes the scratch file when the calibration run ends, successfully or not.
struct ScratchFile {
    path: PathBuf,
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        // Best effort: a failure to clean up must not mask the measurement's
        // own result, and there is nowhere useful to propagate it to.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The default scratch file location: the platform temp directory.
#[must_use]
pub fn default_file() -> PathBuf {
    std::env::temp_dir().join("moonpool-calibrate.scratch")
}

/// Measure read, write and sync latency against `path`.
///
/// `warmup` unrecorded iterations run before each operation's `samples`
/// recorded iterations. The scratch file is created, filled and removed by this
/// function.
///
/// # Errors
///
/// Returns any I/O error from creating, filling, reading, writing or syncing
/// the scratch file.
pub fn measure(path: &Path, samples: u64, warmup: u64) -> std::io::Result<StorageMeasurements> {
    let _guard = ScratchFile {
        path: path.to_path_buf(),
    };

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    prepare(&mut file)?;

    let mut blocks = BlockPicker::new();
    let mut buffer = vec![0_u8; BLOCK_SIZE];
    let payload = payload_block();

    let mut measurements = StorageMeasurements {
        read: Latencies::new("read"),
        write: Latencies::new("write"),
        sync: Latencies::new("sync"),
    };

    let mut checksum = 0_u64;

    for _ in 0..warmup {
        checksum ^= read_once(&mut file, blocks.next(), &mut buffer)?.1;
    }
    for _ in 0..samples {
        let (elapsed, folded) = read_once(&mut file, blocks.next(), &mut buffer)?;
        measurements.read.record(elapsed);
        checksum ^= folded;
    }

    for _ in 0..warmup {
        write_once(&mut file, blocks.next(), &payload)?;
    }
    for _ in 0..samples {
        let elapsed = write_once(&mut file, blocks.next(), &payload)?;
        measurements.write.record(elapsed);
    }

    for _ in 0..warmup {
        sync_once(&mut file, blocks.next(), &payload)?;
    }
    for _ in 0..samples {
        let elapsed = sync_once(&mut file, blocks.next(), &payload)?;
        measurements.sync.record(elapsed);
    }

    // Keep the read work observable so the optimiser cannot elide it.
    std::hint::black_box(checksum);

    Ok(measurements)
}

/// Fill the scratch file with `BLOCK_COUNT` blocks and flush it, untimed, so
/// that later reads land on real data rather than a hole.
fn prepare(file: &mut File) -> std::io::Result<()> {
    let block = payload_block();
    file.seek(SeekFrom::Start(0))?;
    for _ in 0..BLOCK_COUNT {
        file.write_all(&block)?;
    }
    file.sync_all()
}

/// A non-trivial, deterministic block pattern.
fn payload_block() -> Vec<u8> {
    (0..BLOCK_SIZE)
        .map(|index| u8::try_from(index % 251).unwrap_or(0))
        .collect()
}

/// Time one block read and fold the bytes into a checksum.
fn read_once(
    file: &mut File,
    block: u64,
    buffer: &mut [u8],
) -> std::io::Result<(std::time::Duration, u64)> {
    file.seek(SeekFrom::Start(block * BLOCK_SIZE as u64))?;
    let start = Instant::now();
    file.read_exact(buffer)?;
    let elapsed = start.elapsed();

    // Consume the bytes: without this the read buffer is dead and both the
    // compiler and the kernel's readahead heuristics see different work.
    let mut folded = 0_u64;
    for chunk in buffer.chunks(8) {
        folded = folded.rotate_left(7) ^ u64::from(chunk[0]) ^ u64::from(chunk[chunk.len() - 1]);
    }
    Ok((elapsed, folded))
}

/// Time one block write into the already-allocated file region.
fn write_once(file: &mut File, block: u64, payload: &[u8]) -> std::io::Result<std::time::Duration> {
    file.seek(SeekFrom::Start(block * BLOCK_SIZE as u64))?;
    let start = Instant::now();
    file.write_all(payload)?;
    Ok(start.elapsed())
}

/// Dirty one block untimed, then time `sync_all` on its own.
fn sync_once(file: &mut File, block: u64, payload: &[u8]) -> std::io::Result<std::time::Duration> {
    file.seek(SeekFrom::Start(block * BLOCK_SIZE as u64))?;
    file.write_all(payload)?;
    let start = Instant::now();
    file.sync_all()?;
    Ok(start.elapsed())
}

/// A tiny deterministic PRNG picking which block each operation touches.
///
/// Deliberately local: moonpool's seeded randomness belongs to the simulation,
/// not to the measurement of the machine the simulation imitates.
struct BlockPicker {
    state: u64,
}

impl BlockPicker {
    fn new() -> Self {
        Self {
            state: 0x2545_f491_4f6c_dd1d,
        }
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_f491_4f6c_dd1d) % BLOCK_COUNT
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_COUNT, BlockPicker, default_file, measure, payload_block};

    #[test]
    fn block_picker_stays_in_range_and_is_deterministic() {
        let mut first = BlockPicker::new();
        let mut second = BlockPicker::new();
        for _ in 0..10_000 {
            let block = first.next();
            assert!(block < BLOCK_COUNT);
            assert_eq!(block, second.next());
        }
    }

    #[test]
    fn payload_block_is_not_all_zeroes() {
        let block = payload_block();
        assert_eq!(block.len(), super::BLOCK_SIZE);
        assert!(block.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn default_file_lives_in_the_temp_directory() {
        let path = default_file();
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("moonpool-calibrate.scratch")
        );
    }

    #[test]
    fn measure_records_every_operation_and_cleans_up() {
        let path = std::env::temp_dir().join("moonpool-calibrate-test-measure.scratch");
        let measurements = measure(&path, 8, 2).expect("calibration run");

        assert_eq!(measurements.read.count(), 8);
        assert_eq!(measurements.write.count(), 8);
        assert_eq!(measurements.sync.count(), 8);

        // No latency assertions here on purpose: this test proves the plumbing,
        // not the speed of whatever machine happens to run it.
        for summary in [
            measurements.read.summary(),
            measurements.write.summary(),
            measurements.sync.summary(),
        ] {
            let bounds = summary.bounds();
            assert!(bounds.start <= bounds.end);
        }

        assert!(!path.exists(), "scratch file should have been removed");
    }

    #[test]
    fn scratch_file_is_removed_even_when_the_run_fails() {
        // A directory that does not exist makes `open` fail, after the guard
        // has been armed.
        let path = std::env::temp_dir()
            .join("moonpool-calibrate-missing-dir")
            .join("scratch");
        assert!(measure(&path, 1, 0).is_err());
        assert!(!path.exists());
    }
}
