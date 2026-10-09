//! The journal's fault-injection port: the points inside a commit where a
//! power loss leaves the most to recover, asked by the commit itself.
//!
//! A crash from outside lands at a random instant, and a commit's unsynced
//! window is a few microseconds wide, so torn records, records rebuilt
//! from their entries and an ambiguous last batch would almost never be
//! recovered. [`Journal::commit`](crate::Journal::commit) calls
//! [`CommitHooks::at`] at each [`CommitPoint`], with writes issued and not
//! yet synced. A simulation answers by cutting the process's power there
//! (moonpool-sim's `SelfCrash`): the kill lands before the commit's next
//! storage completion, and every unsynced sector resolves by the disk's
//! crash physics. Production passes [`NoCommitHooks`], which does nothing.
//!
//! The decision point is in the shipped code; the simulation only answers.

/// A point inside a commit (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitPoint {
    /// The batch's entries are written, nothing of the batch is synced.
    EntriesWritten,
    /// The batch's persist records are written, and the sync that covers
    /// them has not run.
    RecordsWritten,
    /// The batch is durable, and its metainfo update has not started.
    BeforeMeta,
}

/// The journal's fault-injection hooks (see the module doc).
pub trait CommitHooks: Send + Sync + 'static {
    /// A commit reached `point`. The default does nothing.
    fn at(&self, point: CommitPoint) {
        let _ = point;
    }
}

/// The production hooks: nothing happens at any point.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCommitHooks;

impl CommitHooks for NoCommitHooks {}
