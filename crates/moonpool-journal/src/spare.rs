//! The spare segment: the next segment file, prepared before a commit needs
//! it, so that a roll is a header write and a rename instead of a zero-fill
//! of a whole segment (`LogCabin`'s segment preparer does the same).
//!
//! A spare is `spare-<generation>.wal`: full size, zero-filled, synced, and
//! its directory entry synced. It holds no header, so no open ever reads it
//! as a segment. Only the open that prepared it uses it, and only once it
//! knows the preparation finished: every open deletes the spares it finds,
//! whole or half-written, before it writes anything. A roll writes the
//! header into the spare, syncs it, renames it to its segment name and syncs
//! the directory (see [`Segment::adopt`](crate::segment::Segment::adopt)), so
//! a segment file still either does not exist or has its full size and
//! both headers.
//!
//! The spare is zero-filled, not preallocated. An unwritten extent
//! (`fallocate`) turns each first write to a block into a metadata change,
//! so every commit's `fdatasync` would have file metadata to write again.
//! Writing the zeros once, off the commit path, keeps a segment's syncs pure
//! data syncs.

use std::sync::{Arc, Mutex};

use moonpool_core::{
    BlockFile, Detach, LocalStorageProvider, LocalTaskProvider, OpenOptions, TaskProvider,
};

use crate::format::{BLOCK, BLOCK_U64, Geometry, usize_of};
use crate::io::write_blocks;

/// Blocks zeroed per write while preparing a segment file.
const FILL_BLOCKS: u64 = 256;

const PREFIX: &str = "spare-";
const SUFFIX: &str = ".wal";

/// How often an active preparation point fires. A preparation runs once per
/// roll, so the rate is the commit's.
const SPARE_HINT_PROB: f64 = 0.25;

/// `spare-0000000007-0000000003.wal`: the fourth spare of open generation 7.
/// Every spare has its own name, so a preparation still running or cleaning
/// up (an earlier open's, or a failed one of this open) never touches the
/// file a roll uses.
pub(crate) fn spare_name(generation: u32, number: u32) -> String {
    format!("{PREFIX}{generation:010}-{number:010}{SUFFIX}")
}

/// Whether `name` is a spare, of any generation.
pub(crate) fn is_spare(name: &str) -> bool {
    let digits = |part: &str| part.len() == 10 && part.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(generation, number)| digits(generation) && digits(number))
}

/// Where a spare's preparation stands, shared with the task preparing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Being written.
    Preparing,
    /// Full size, zero-filled, synced, its name synced: a roll may use it.
    Ready,
    /// The preparation failed; the file (if any) is not to be trusted.
    Failed,
    /// The journal is gone: the preparation stops at its next step.
    Abandoned,
}

/// The spare of this open.
pub(crate) struct Spare {
    pub name: String,
    pub progress: Arc<Mutex<Progress>>,
}

impl Spare {
    pub fn new(name: String) -> Self {
        Self {
            name,
            progress: Arc::new(Mutex::new(Progress::Preparing)),
        }
    }

    pub fn progress(&self) -> Progress {
        *self
            .progress
            .lock()
            .expect("spare lock poisoned: prior task panicked")
    }
}

/// The journal let go of this spare (it is gone, or it rolled past it): a
/// preparation still running stops at its next step. A ready or failed
/// spare is left as it is, for the next open to delete.
impl Drop for Spare {
    fn drop(&mut self) {
        let mut progress = self
            .progress
            .lock()
            .expect("spare lock poisoned: prior task panicked");
        if *progress == Progress::Preparing {
            *progress = Progress::Abandoned;
        }
    }
}

/// One spare segment to prepare: plain data, so a spawner builds the
/// preparation future itself, on its own runtime, and nothing is boxed.
///
/// A [`SpareSpawner`] receives it and runs [`SpareJob::run`] in the
/// background.
pub struct SpareJob<P> {
    provider: P,
    dir: String,
    name: String,
    geometry: Geometry,
    progress: Arc<Mutex<Progress>>,
}

impl<P: LocalStorageProvider> SpareJob<P> {
    /// Create, zero-fill and sync the spare, then mark it ready for the
    /// journal's next roll. A failure is recorded for the journal (the next
    /// roll then creates its segment inline) and the half-written file is
    /// removed; nothing is returned.
    ///
    /// The future is `Send` exactly when `P`'s futures are, so a spawner on a
    /// work-stealing runtime can spawn it for a `Send` provider.
    pub async fn run(self) {
        // A failure is recorded in the spare's progress.
        let _ = prepare(
            self.provider,
            self.dir,
            self.name,
            self.geometry,
            self.progress,
        )
        .await;
    }
}

/// Where a journal prepares its spare segments in the background: a type
/// parameter of [`Journal`](crate::Journal), so the preparation future keeps
/// its concrete type (and its `Send`-ness) and is never boxed.
///
/// - [`NoSpares`], the default: no background preparation. A roll creates
///   its segment inline, or [`Journal::prepare_spare`](crate::Journal::prepare_spare)
///   prepares one when the caller has idle time.
/// - [`Spares`]: a [`TaskProvider`] (Tokio, the simulator), for a `Send`
///   provider. Set with [`Journal::prepare_spares_with`](crate::Journal::prepare_spares_with).
/// - [`LocalSpares`]: a [`LocalTaskProvider`] (compio, the simulator), for
///   any provider. Set with [`Journal::prepare_spares_local`](crate::Journal::prepare_spares_local).
pub trait SpareSpawner<P> {
    /// Whether this spawner prepares spares at all. A journal with a spawner
    /// that does not never starts one, and never names one.
    const BACKGROUND: bool;

    /// Run `job` in the background: [`SpareJob::run`], detached.
    fn spawn(&self, job: SpareJob<P>);
}

/// No background spare preparation: the default [`SpareSpawner`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoSpares;

impl<P> SpareSpawner<P> for NoSpares {
    const BACKGROUND: bool = false;

    fn spawn(&self, _job: SpareJob<P>) {}
}

/// Prepare spares on a [`TaskProvider`]: the preparation future is `Send`
/// because the provider is.
#[derive(Clone, Debug)]
pub struct Spares<T>(pub T);

impl<P: moonpool_core::StorageProvider, T: TaskProvider> SpareSpawner<P> for Spares<T> {
    const BACKGROUND: bool = true;

    fn spawn(&self, job: SpareJob<P>) {
        self.0.spawn_task("journal spare", job.run()).detach();
    }
}

/// Prepare spares on a [`LocalTaskProvider`]: the preparation runs on the
/// spawning thread, so neither the provider nor its futures need be `Send`.
#[derive(Clone, Debug)]
pub struct LocalSpares<T>(pub T);

impl<P: LocalStorageProvider, T: LocalTaskProvider> SpareSpawner<P> for LocalSpares<T> {
    const BACKGROUND: bool = true;

    fn spawn(&self, job: SpareJob<P>) {
        self.0.spawn_local_task("journal spare", job.run()).detach();
    }
}

impl<P> SpareJob<P> {
    /// The preparation of `spare` in `dir`.
    pub(crate) fn new(provider: P, dir: String, spare: &Spare, geometry: Geometry) -> Self {
        Self {
            provider,
            dir,
            name: spare.name.clone(),
            geometry,
            progress: Arc::clone(&spare.progress),
        }
    }
}

/// Prepare the spare `name` in `dir`, then mark it [`Progress::Ready`]. A
/// failure marks it [`Progress::Failed`] and removes the file: the next roll
/// creates its segment inline, as if there were no spare.
///
/// # Errors
/// The provider's error, or [`std::io::ErrorKind::Interrupted`] when the
/// journal abandoned the spare first.
pub(crate) async fn prepare<P: LocalStorageProvider>(
    provider: P,
    dir: String,
    name: String,
    geometry: Geometry,
    progress: Arc<Mutex<Progress>>,
) -> std::io::Result<()> {
    let path = format!("{dir}/{name}");
    let abandoned = || {
        *progress
            .lock()
            .expect("spare lock poisoned: prior task panicked")
            == Progress::Abandoned
    };
    let prepared = write_spare(&provider, &dir, &path, geometry, &abandoned).await;
    let outcome = {
        let mut state = progress
            .lock()
            .expect("spare lock poisoned: prior task panicked");
        match prepared {
            Ok(true) if *state == Progress::Preparing => {
                *state = Progress::Ready;
                Ok(())
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the journal abandoned its spare segment",
            )),
            Err(error) => {
                if *state == Progress::Preparing {
                    *state = Progress::Failed;
                }
                Err(error)
            }
        }
    };
    if outcome.is_err() {
        // Best effort: whatever is left is deleted by the next open.
        let _ = provider.delete(&path).await;
    }
    outcome
}

/// Create, zero-fill and sync the file at `path`, then sync its name.
/// `Ok(false)` when `abandoned` stopped it first.
async fn write_spare<P: LocalStorageProvider>(
    provider: &P,
    dir: &str,
    path: &str,
    geometry: Geometry,
    abandoned: &impl Fn() -> bool,
) -> std::io::Result<bool> {
    if provider.exists(path).await? {
        provider.delete(path).await?;
    }
    let file = provider
        .open(path, OpenOptions::create_new_write().read(true))
        .await?;
    let file = BlockFile::new(file, BLOCK)?;
    if !zero_fill(&file, geometry.total_blocks(), abandoned).await? {
        return Ok(false);
    }
    // A power loss here leaves a spare of any length, any of its zeros lost.
    moonpool_buggify::hint!("journal spare: zero-filled, not synced", SPARE_HINT_PROB).await;
    // Bytes and length alike: a segment never changes size again.
    file.sync().await?;
    drop(file);
    // A power loss here may lose the spare's name: the file is synced, but
    // nothing names it yet.
    moonpool_buggify::hint!(
        "journal spare: synced, its name not synced",
        SPARE_HINT_PROB
    )
    .await;
    if abandoned() {
        return Ok(false);
    }
    provider.sync_dir(dir).await?;
    Ok(true)
}

/// Write `total` blocks of zeros from block 0, `FILL_BLOCKS` per write.
/// `Ok(false)` when `abandoned` stopped it first.
pub(crate) async fn zero_fill<F: moonpool_core::LocalStorageFile>(
    file: &BlockFile<F>,
    total: u64,
    abandoned: &impl Fn() -> bool,
) -> std::io::Result<bool> {
    // One zeroed buffer for the whole fill, moved into each write and back:
    // never written to, so it stays zeros, and never copied.
    let mut chunk = file.buffer(usize_of(FILL_BLOCKS))?;
    let mut block = 0;
    while block < total {
        if abandoned() {
            return Ok(false);
        }
        let count = FILL_BLOCKS.min(total - block);
        chunk.truncate(usize_of(count * BLOCK_U64));
        chunk = write_blocks(file, block, chunk).await?;
        block += count;
    }
    assert_eq!(block, total, "the fill covers the file exactly");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spare_names_are_never_segment_names() {
        assert!(is_spare(&spare_name(7, 3)));
        assert!(!is_spare("spare-7-3.wal"));
        assert_eq!(crate::segment::parse_segment_name(&spare_name(7, 3)), None);
        assert!(!crate::segment::is_segment_leftover(&spare_name(7, 3)));
        assert!(!is_spare(&crate::segment::segment_name(7)));
    }
}
