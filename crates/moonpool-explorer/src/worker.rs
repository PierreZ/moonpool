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
//! worker just before `_exit`, so a crashed worker leaves an empty journal
//! (its recipe is still recorded as a crash reproducer by the controller).
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

/// Byte size of one journal result slot.
const SLOT_SIZE: usize = 8 + MAX_JOURNAL_ENTRIES * std::mem::size_of::<RawEvent>();

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

/// How a finished worker run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerExit {
    /// Run completed and reported success.
    Ok,
    /// Run completed and reported a simulation failure (exit code 42).
    Failed,
    /// Worker died without reporting (panic, abort, signal).
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

    /// Zero a slot before handing it to a worker.
    pub fn clear_slot(&self, idx: usize) {
        // Safety: slot_ptr is in bounds for SLOT_SIZE bytes.
        unsafe { std::ptr::write_bytes(self.slot_ptr(idx), 0, SLOT_SIZE) };
    }

    /// Serialize the given journal into a slot (called in the worker).
    pub fn write_slot(&self, idx: usize, journal: &[DiscoveryEvent]) {
        let ptr = self.slot_ptr(idx);
        let len = journal.len().min(MAX_JOURNAL_ENTRIES);
        // Safety: the slot is SLOT_SIZE bytes: an 8-byte header followed by
        // MAX_JOURNAL_ENTRIES RawEvents; len is capped to that entry count.
        // Write the entries before publishing len so a worker interrupted
        // during serialization still leaves the controller an empty slot.
        unsafe {
            let entries = ptr.add(8).cast::<()>().cast::<RawEvent>();
            for (i, event) in journal.iter().take(len).enumerate() {
                entries.add(i).write(RawEvent {
                    call_count: event.call_count,
                    state_id: event.state_id,
                    kind: event.kind as u64,
                });
            }
            *ptr.cast::<()>().cast::<u32>() =
                u32::try_from(len).expect("len capped at MAX_JOURNAL_ENTRIES");
        }
    }

    /// Deserialize a slot's journal (called in the controller after reaping).
    pub fn read_slot(&self, idx: usize) -> Vec<DiscoveryEvent> {
        let ptr = self.slot_ptr(idx);
        // Safety: the slot layout matches write_slot; len is re-capped on read
        // so a corrupted header cannot walk out of the slot.
        unsafe {
            let len = (*ptr.cast::<()>().cast::<u32>()) as usize;
            let len = len.min(MAX_JOURNAL_ENTRIES);
            let entries = ptr.add(8).cast::<()>().cast::<RawEvent>();
            (0..len)
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
                .collect()
        }
    }
}

/// Wait for any child process to exit, retrying on `EINTR`.
///
/// Returns `(pid, status)` or `None` if there are no children left.
#[cfg(unix)]
pub(crate) fn wait_any() -> Option<(libc::pid_t, libc::c_int)> {
    let mut status: libc::c_int = 0;
    loop {
        // Safety: waitpid with -1 waits for any child; status is a valid out-pointer.
        let pid = unsafe { libc::waitpid(-1, &raw mut status, 0) };
        if pid > 0 {
            return Some((pid, status));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

/// Classify a `waitpid` status into a [`WorkerExit`].
#[cfg(unix)]
pub(crate) fn classify_exit(status: libc::c_int) -> WorkerExit {
    if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => WorkerExit::Ok,
            42 => WorkerExit::Failed,
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
        ];
        pool.clear_slot(0);
        pool.clear_slot(1);
        pool.write_slot(1, &journal);

        assert!(pool.read_slot(0).is_empty());
        assert_eq!(pool.read_slot(1), journal);

        // Clearing wipes the journal.
        pool.clear_slot(1);
        assert!(pool.read_slot(1).is_empty());
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
