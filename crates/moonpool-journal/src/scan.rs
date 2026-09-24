//! The recovery decision: given what the walk found at every index of a
//! segment, what to keep, what to report, and what to cut.
//!
//! | Entry | Slot      | Mid-log (a good entry follows)   | Tail (nothing good follows) |
//! |-------|-----------|----------------------------------|-----------------------------|
//! | good  | valid     | keep                             | —                           |
//! | good  | empty/bad | keep, rewrite the slot           | —                           |
//! | bad   | valid     | corrupt: report (index, epoch)   | torn, ambiguous: report     |
//! | bad   | empty     | refuse: an entry went missing    | torn tail: truncate         |
//! | bad   | bad       | refuse: double fault             | torn tail: truncate         |
//!
//! "Tail" means the **last batch**. Only one batch is ever unsynced, and a
//! crash resolves each of its sectors independently — so inside it, entry
//! *i* may be torn while entry *i + 1* survived, which is a crash, not
//! corruption. The last good entry records its position in its batch, which
//! gives the batch's first index; the tail starts at the first index from
//! there on that is not good, and everything past the last good entry is tail
//! too. Before the last batch, every index was synced, so a bad entry there
//! is damage, never a crash. A single-node journal cannot tell
//! whether an ambiguous tail entry (valid slot, bad entry) was acknowledged,
//! so it truncates it like a torn write — but it still reports it, so a
//! replicated caller can keep it if it was committed. A sealed segment has no
//! tail at all: the next segment exists, so its every index was synced before
//! the rollover and anything short of "good" or "corrupt" is refused.
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
    /// `None` only below the journal's live start, where nothing is checked.
    pub recs: Vec<Option<Rec>>,
    /// Kept indexes whose slot on disk must be rewritten from the entry.
    pub rewrite_slots: Vec<u64>,
    /// Damaged entries mid-log, as `(index, epoch)`.
    pub corrupt: Vec<(u64, u64)>,
    /// Torn tail entries whose slot was intact, as `(index, epoch)`.
    pub ambiguous_tail: Vec<(u64, u64)>,
    /// Whether anything past the kept range was found and must be wiped.
    pub torn: bool,
}

/// Decide a segment whose indexes start at `first`.
///
/// `found` covers every index the walk reached. `sealed` is true when a later
/// segment exists, in which case `found` must cover the segment's whole range.
/// Indexes below `start` are compacted away: kept for addressing, never
/// judged.
///
/// # Errors
///
/// [`JournalError::MissingEntry`] or [`JournalError::DoubleFault`] for an
/// unrecoverable index that is not part of a torn tail.
pub(crate) fn decide(
    first: u64,
    start: u64,
    found: &[Found],
    sealed: bool,
) -> Result<Decision, JournalError> {
    let kept = if sealed {
        found.len()
    } else {
        match found.iter().rposition(|f| f.entry.is_some()) {
            None => 0,
            Some(last) => {
                let (_, header) = found[last].entry.expect("the last good entry");
                let batch_start = last.saturating_sub(header.batch_pos as usize);
                (batch_start..=last)
                    .find(|rel| found[*rel].entry.is_none())
                    .unwrap_or(last + 1)
            }
        }
    };
    let mut decision = Decision::default();
    for (rel, f) in found.iter().enumerate() {
        let index = first + rel as u64;
        if rel >= kept {
            decision.torn |= f.slot != SlotState::Empty;
            if let SlotState::Valid(slot) = f.slot {
                decision.ambiguous_tail.push((index, slot.epoch));
            }
            continue;
        }
        let rec = match (f.entry, f.slot) {
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
                Some(Rec {
                    slot: rebuilt,
                    corrupt: false,
                })
            }
            (None, SlotState::Valid(slot)) => {
                if index >= start {
                    decision.corrupt.push((index, slot.epoch));
                }
                Some(Rec {
                    slot,
                    corrupt: true,
                })
            }
            (None, _) if index < start => None,
            (None, SlotState::Empty) => return Err(JournalError::MissingEntry { index }),
            (None, SlotState::Bad) => return Err(JournalError::DoubleFault { index }),
        };
        decision.recs.push(rec);
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
                    batch_pos: 0,
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
        let decision = decide(1, 1, &found, false).expect("recoverable");
        assert_eq!(decision.recs.len(), 3);
        assert_eq!(decision.rewrite_slots, vec![2, 3]);
        assert!(!decision.torn);
    }

    #[test]
    fn a_bad_entry_with_a_valid_slot_mid_log_is_corrupt() {
        let found = [
            good(1, SlotState::Valid(slot(1))),
            bad(SlotState::Valid(slot(2))),
            good(3, SlotState::Valid(slot(3))),
        ];
        let decision = decide(1, 1, &found, false).expect("recoverable");
        assert_eq!(decision.corrupt, vec![(2, 5)]);
        assert!(decision.recs[1].expect("kept").corrupt);
    }

    #[test]
    fn the_tail_is_truncated_and_its_intact_slots_reported() {
        let found = [
            good(1, SlotState::Valid(slot(1))),
            bad(SlotState::Valid(slot(2))),
            bad(SlotState::Empty),
            bad(SlotState::Bad),
        ];
        let decision = decide(1, 1, &found, false).expect("recoverable");
        assert_eq!(decision.recs.len(), 1);
        assert_eq!(decision.ambiguous_tail, vec![(2, 5)]);
        assert!(decision.torn);
    }

    #[test]
    fn a_hole_inside_the_last_batch_is_a_torn_write() {
        // Indexes 2..=4 were one unsynced batch: 3 was torn, 4 survived.
        let mut last = good(4, SlotState::Bad);
        if let Some((_, header)) = &mut last.entry {
            header.batch_pos = 2;
        }
        let found = [
            good(1, SlotState::Valid(slot(1))),
            good(2, SlotState::Bad),
            bad(SlotState::Bad),
            last,
        ];
        let decision = decide(1, 1, &found, false).expect("a crash, not corruption");
        assert_eq!(decision.recs.len(), 2);
        assert!(decision.torn);
    }

    #[test]
    fn mid_log_loss_is_refused() {
        let missing = [bad(SlotState::Empty), good(2, SlotState::Valid(slot(2)))];
        assert!(matches!(
            decide(1, 1, &missing, false),
            Err(JournalError::MissingEntry { index: 1 })
        ));
        let double = [bad(SlotState::Bad), good(2, SlotState::Valid(slot(2)))];
        assert!(matches!(
            decide(1, 1, &double, false),
            Err(JournalError::DoubleFault { index: 1 })
        ));
    }

    #[test]
    fn a_sealed_segment_has_no_tail() {
        let found = [good(1, SlotState::Valid(slot(1))), bad(SlotState::Bad)];
        assert!(matches!(
            decide(1, 1, &found, true),
            Err(JournalError::DoubleFault { index: 2 })
        ));
    }

    #[test]
    fn indexes_below_the_start_are_not_judged() {
        let found = [bad(SlotState::Bad), good(2, SlotState::Empty)];
        let decision = decide(1, 2, &found, false).expect("compacted index ignored");
        assert_eq!(decision.recs[0], None);
    }
}
