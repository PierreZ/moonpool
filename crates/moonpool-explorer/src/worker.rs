//! Bounded worker-process pool.
//!
//! The controller executes exploration jobs through a fixed-size pool of
//! forked worker processes. Each worker runs **exactly one** timeline (a
//! replay of the job's recipe plus a fresh continuation), reports its
//! observations through a `MAP_SHARED` result slot, and exits. Workers never
//! fork, never touch the frontier, and never make exploration decisions —
//! the number of live processes is therefore `1 + workers` at all times, no
//! matter how large the logical exploration space grows.
//!
//! ```text
//! controller ──fork──▶ worker (slot 0)  ──runs one timeline──▶ _exit
//!            ──fork──▶ worker (slot 1)  ──runs one timeline──▶ _exit
//!            ◀─waitpid─ merge slot journal, decide novelty, expand frontier
//! ```
//!
//! # Result slot layout
//!
//! Each worker owns one slot in a `MAP_SHARED` region:
//!
//! ```text
//! [len: u32][padding: u32][entries: [RawEvent; MAX_JOURNAL_ENTRIES]]
//! RawEvent = { call_count: u64, state_id: u64, kind: u64 }
//! ```
//!
//! The slot is cleared by the controller before each fork and written by the
//! worker just before `_exit`. A worker catches a panic of its run and still
//! writes its journal; a worker that dies (abort, signal) leaves an empty
//! journal. In both cases the controller records the recipe of the job as a
//! crash reproducer.
//! Sanitizer-coverage counters travel through the per-slot sancov pool (see
//! [`crate::sancov`]).
//!
//! Everything here is portable POSIX (`fork`, `waitpid`, `mmap MAP_SHARED`)
//! — no Linux-only interfaces — and the controller forks at a quiescent
//! point between runs, never mid-simulation.

use std::cell::Cell;

use crate::journal::{DiscoveryEvent, MAX_JOURNAL_ENTRIES};
use crate::shared_mem::SharedMemory;
use moonpool_assertions::DiscoveryKind;

/// One serialized discovery event in a result slot.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawEvent {
    call_count: u64,
    state_id: u64,
    kind: u64,
}

/// Byte size of a slot header: the entry count (`u32`, padded to 8) and the
/// run's final RNG call count (`u64`).
const SLOT_HEADER: usize = 16;

/// Byte size of one journal result slot.
const SLOT_SIZE: usize = SLOT_HEADER + MAX_JOURNAL_ENTRIES * std::mem::size_of::<RawEvent>();

thread_local! {
    /// True in a forked worker process.
    static IS_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current process is a forked exploration worker.
#[must_use]
pub fn explorer_is_child() -> bool {
    IS_WORKER.with(Cell::get)
}

/// Mark the current process as a worker (called in the child after fork).
pub(crate) fn enter_worker() {
    IS_WORKER.with(|w| w.set(true));
}

/// Exit code of a worker whose run completed and passed.
#[cfg(unix)]
pub(crate) const EXIT_OK: libc::c_int = 0;

/// Exit code of a worker whose run completed and reported a failure.
#[cfg(unix)]
pub(crate) const EXIT_FAILED: libc::c_int = 42;

/// Exit code of a worker whose run panicked. The controller classifies it as
/// [`WorkerExit::Crashed`], like any other code that is not [`EXIT_OK`] or
/// [`EXIT_FAILED`].
#[cfg(unix)]
pub(crate) const EXIT_PANICKED: libc::c_int = 43;

/// How a finished worker run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerExit {
    /// Run completed and reported success.
    Ok,
    /// Run completed and reported a simulation failure (exit code 42).
    Failed,
    /// Worker panicked ([`EXIT_PANICKED`]) or died without reporting
    /// (abort, signal, wall-clock ceiling).
    Crashed,
}

/// Fixed-size pool of `MAP_SHARED` journal result slots.
pub(crate) struct SlotPool {
    memory: SharedMemory,
    slots: usize,
}

impl SlotPool {
    /// Allocate `slots` journal result slots in shared memory.
    pub fn new(slots: usize) -> Result<Self, std::io::Error> {
        Ok(Self {
            memory: SharedMemory::array(slots, SLOT_SIZE)?,
            slots,
        })
    }

    fn slot_ptr(&self, idx: usize) -> *mut u8 {
        assert!(idx < self.slots, "slot index out of range");
        // Safety: base points to slots * SLOT_SIZE bytes and idx < slots.
        unsafe { self.memory.as_ptr().add(idx * SLOT_SIZE) }
    }

    /// A slot's `len` header, its final-count header and the start of its
    /// entry array.
    fn slot_parts(&self, idx: usize) -> (*mut u32, *mut u64, *mut RawEvent) {
        let ptr = self.slot_ptr(idx);
        // Safety: the slot is SLOT_SIZE bytes: a SLOT_HEADER-byte header (len
        // at 0, the final count at 8) followed by MAX_JOURNAL_ENTRIES
        // RawEvents.
        let (end_count, entries) = unsafe { (ptr.add(8), ptr.add(SLOT_HEADER)) };
        (
            ptr.cast::<()>().cast::<u32>(),
            end_count.cast::<()>().cast::<u64>(),
            entries.cast::<()>().cast::<RawEvent>(),
        )
    }

    /// Zero a slot before handing it to a worker.
    pub fn clear_slot(&self, idx: usize) {
        // Safety: slot_ptr is in bounds for SLOT_SIZE bytes.
        unsafe { std::ptr::write_bytes(self.slot_ptr(idx), 0, SLOT_SIZE) };
    }

    /// Serialize the given journal and the run's final RNG call count into a
    /// slot (called in the worker).
    pub fn write_slot(&self, idx: usize, journal: &[DiscoveryEvent], end_count: u64) {
        let (header, end, entries) = self.slot_parts(idx);
        let len = journal.len().min(MAX_JOURNAL_ENTRIES);
        // Safety: len is capped to the slot's MAX_JOURNAL_ENTRIES entries.
        // Write the entries before publishing len so a worker interrupted
        // during serialization still leaves the controller an empty slot.
        unsafe {
            for (i, event) in journal.iter().take(len).enumerate() {
                entries.add(i).write(RawEvent {
                    call_count: event.call_count,
                    state_id: event.state_id,
                    kind: event.kind as u64,
                });
            }
            end.write(end_count);
            *header = u32::try_from(len).expect("len capped at MAX_JOURNAL_ENTRIES");
        }
    }

    /// Deserialize a slot's journal and the run's final RNG call count
    /// (called in the controller after reaping).
    pub fn read_slot(&self, idx: usize) -> (Vec<DiscoveryEvent>, u64) {
        let (header, end, entries) = self.slot_parts(idx);
        // Safety: the slot layout matches write_slot; len is re-capped on read
        // so a corrupted header cannot walk out of the slot.
        unsafe {
            let len = (*header as usize).min(MAX_JOURNAL_ENTRIES);
            let events = (0..len)
                .filter_map(|i| {
                    let raw = entries.add(i).read();
                    let kind = u8::try_from(raw.kind)
                        .ok()
                        .and_then(DiscoveryKind::from_u8)?;
                    Some(DiscoveryEvent {
                        call_count: raw.call_count,
                        kind,
                        state_id: raw.state_id,
                    })
                })
                .collect();
            (events, end.read())
        }
    }
}

/// What one non-blocking look at a worker found.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerState {
    /// Still running.
    Running,
    /// Exited with this `waitpid` status; it has been reaped.
    Exited(libc::c_int),
    /// No such child of this process any more (reaped elsewhere): its
    /// result is lost.
    Gone,
}

/// Check one worker without blocking, retrying on `EINTR`.
///
/// Waits on `pid` specifically, never on `-1`: a controller embedded in a
/// larger harness must not reap children it did not spawn.
#[cfg(unix)]
pub(crate) fn poll_worker(pid: libc::pid_t) -> WorkerState {
    let mut status: libc::c_int = 0;
    loop {
        // Safety: waitpid on a specific pid with WNOHANG; status is a valid
        // out-pointer.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if reaped == pid {
            return WorkerState::Exited(status);
        }
        if reaped == 0 {
            return WorkerState::Running;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return WorkerState::Gone;
        }
    }
}

/// Kill a worker with `SIGKILL` and reap it, returning its `waitpid` status
/// (`None` if it was already gone).
#[cfg(unix)]
pub(crate) fn kill_worker(pid: libc::pid_t) -> Option<libc::c_int> {
    // Safety: signalling a pid this controller forked and has not yet reaped,
    // so the pid cannot have been recycled for another process.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut status: libc::c_int = 0;
    loop {
        // Safety: blocking waitpid on the specific pid just killed; SIGKILL
        // cannot be caught, so this returns promptly.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if reaped == pid {
            return Some(status);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

/// Classify a `waitpid` status into a [`WorkerExit`].
#[cfg(unix)]
pub(crate) fn classify_exit(status: libc::c_int) -> WorkerExit {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            EXIT_OK => WorkerExit::Ok,
            EXIT_FAILED => WorkerExit::Failed,
            _ => WorkerExit::Crashed,
        }
    } else {
        WorkerExit::Crashed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_roundtrip() {
        let pool = SlotPool::new(2).expect("alloc slots");
        let journal = vec![
            DiscoveryEvent {
                call_count: 17,
                kind: DiscoveryKind::SometimesPass,
                state_id: 0xabcd,
            },
            DiscoveryEvent {
                call_count: 99,
                kind: DiscoveryKind::BucketQuality,
                state_id: 42,
            },
            DiscoveryEvent {
                call_count: 123,
                kind: DiscoveryKind::BooleanCombination,
                state_id: 77,
            },
        ];
        pool.clear_slot(0);
        pool.clear_slot(1);
        pool.write_slot(1, &journal, 4_096);

        assert_eq!(pool.read_slot(0), (Vec::new(), 0));
        assert_eq!(pool.read_slot(1), (journal, 4_096));

        // Clearing wipes the journal and the final count.
        pool.clear_slot(1);
        assert_eq!(pool.read_slot(1), (Vec::new(), 0));
    }

    #[test]
    fn worker_flag_default_false() {
        assert!(!explorer_is_child());
    }

    #[test]
    fn slot_count_overflow_is_rejected() {
        let Err(error) = SlotPool::new(usize::MAX) else {
            panic!("slot byte size must overflow");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
