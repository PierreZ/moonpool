//! The recovery decision: given what the walk found at each index of a
//! segment, what to keep, what to report, and where the log ends.
//!
//! | Entry | Slot      | Action                                        |
//! |-------|-----------|-----------------------------------------------|
//! | good  | valid     | keep                                          |
//! | good  | empty/bad | keep, rewrite the slot                        |
//! | bad   | valid     | mark corrupt, report `(index, epoch)` upward  |
//! | bad   | empty     | torn tail: the log ends here                  |
//! | bad   | bad       | double fault: refuse to start                 |
//!
//! This is the CLSTORE rule for batched appends (Alagappan et al., FAST '18,
//! §4): the first entry without an identifier ends the log, and every earlier
//! faulty entry that has one is corrupted. The one exception — the *last*
//! entry of the log, whose bad entry beside a valid slot a crash can produce
//! just as well as corruption (the paper's Appendix A) — is applied by the
//! journal, which alone knows which entry is last.
//!
//! Nothing here does I/O; [`Segment::recover`](crate::segment) walks the
//! file and hands the result in.

use crate::JournalError;
use crate::layout::{EntryHeader, Slot, SlotState};

/// What the walk found at one index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    pub slot: SlotState,
    /// The entry, where one checked out: its offset and header.
    pub entry: Option<(u64, EntryHeader)>,
}

/// One kept index, as the segment tracks it in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rec {
    pub slot: Slot,
    /// The entry is damaged but its slot is intact: reads report it.
    pub corrupt: bool,
}

/// The decision for one segment.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Decision {
    /// One record per kept index, starting at the segment's first index.
    pub recs: Vec<Rec>,
    /// Kept indexes whose slot on disk must be rewritten from the entry.
    pub rewrite_slots: Vec<u64>,
    /// Damaged entries whose slot is intact, as `(index, epoch)`.
    pub corrupt: Vec<(u64, u64)>,
    /// The walk met an index with neither an intact entry nor an identifier:
    /// the log ends there.
    pub ended: bool,
}

/// Decide a segment whose indexes start at `first`, from what the walk
/// found. The walk stops at the first index that ends the log, so that index,
/// if any, is the last one in `found`.
///
/// # Errors
///
/// [`JournalError::DoubleFault`] where an entry and its slot are both
/// damaged.
pub(crate) fn decide(first: u64, found: &[Found]) -> Result<Decision, JournalError> {
    let mut decision = Decision::default();
    for (rel, f) in found.iter().enumerate() {
        let index = first + rel as u64;
        match (f.entry, f.slot) {
            (Some((offset, header)), slot) => {
                let rebuilt = Slot {
                    index,
                    epoch: header.epoch,
                    offset: u32::try_from(offset).expect("offsets fit a 32-bit segment"),
                    length: header.length,
                    entry_crc: header.crc,
                };
                if slot != SlotState::Valid(rebuilt) {
                    decision.rewrite_slots.push(index);
                }
                decision.recs.push(Rec {
                    slot: rebuilt,
                    corrupt: false,
                });
            }
            (None, SlotState::Valid(slot)) => {
                decision.corrupt.push((index, slot.epoch));
                decision.recs.push(Rec {
                    slot,
                    corrupt: true,
                });
            }
            (None, SlotState::Empty) => {
                decision.ended = true;
                break;
            }
            (None, SlotState::Bad) => return Err(JournalError::DoubleFault { index }),
        }
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(index: u64) -> Slot {
        Slot {
            index,
            epoch: 5,
            offset: 4096 + 64 * u32::try_from(index).expect("small"),
            length: 8,
            entry_crc: 1,
        }
    }

    fn good(index: u64, slot_state: SlotState) -> Found {
        let s = slot(index);
        Found {
            slot: slot_state,
            entry: Some((
                u64::from(s.offset),
                EntryHeader {
                    length: s.length,
                    index,
                    epoch: s.epoch,
                    crc: s.entry_crc,
                },
            )),
        }
    }

    fn bad(slot_state: SlotState) -> Found {
        Found {
            slot: slot_state,
            entry: None,
        }
    }

    #[test]
    fn good_entries_are_kept_and_lost_slots_rewritten() {
        let found = [
            good(1, SlotState::Valid(slot(1))),
            good(2, SlotState::Empty),
            good(3, SlotState::Bad),
        ];
        let decision = decide(1, &found).expect("recoverable");
        assert_eq!(decision.recs.len(), 3);
        assert_eq!(decision.rewrite_slots, vec![2, 3]);
        assert!(!decision.ended);
    }

    #[test]
    fn a_bad_entry_with_a_valid_slot_is_corrupt() {
        let found = [
            good(1, SlotState::Valid(slot(1))),
            bad(SlotState::Valid(slot(2))),
            good(3, SlotState::Valid(slot(3))),
        ];
        let decision = decide(1, &found).expect("recoverable");
        assert_eq!(decision.corrupt, vec![(2, 5)]);
        assert!(decision.recs[1].corrupt);
    }

    #[test]
    fn the_first_entry_without_an_identifier_ends_the_log() {
        let found = [
            good(1, SlotState::Valid(slot(1))),
            bad(SlotState::Valid(slot(2))),
            bad(SlotState::Empty),
        ];
        let decision = decide(1, &found).expect("recoverable");
        assert_eq!(decision.recs.len(), 2);
        assert_eq!(decision.corrupt, vec![(2, 5)]);
        assert!(decision.ended);
    }

    #[test]
    fn a_double_fault_is_refused() {
        let found = [good(1, SlotState::Valid(slot(1))), bad(SlotState::Bad)];
        assert!(matches!(
            decide(1, &found),
            Err(JournalError::DoubleFault { index: 2 })
        ));
    }
}
