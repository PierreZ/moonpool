//! The spare segment: a dead segment's file, kept for the next roll instead
//! of deleted, so a roll is a header write and a rename, with no byte of a
//! new file written or allocated (PostgreSQL recycles its WAL segments the
//! same way).
//!
//! When the floor or later writes leave the front segment with nothing live,
//! the journal renames it to `spare-<generation>-<n>.wal` and syncs the
//! directory, if it holds no spare yet; else it deletes it. The file keeps
//! its full size, its old header and its old batches, all durable. A roll
//! writes the new header into the spare, syncs it, renames it to its
//! segment name and syncs the directory
//! (see [`Segment::adopt`](crate::segment::Segment::adopt)).
//!
//! **Old bytes are stale, never evidence.** A segment named after its
//! first batch only ever receives batches at or above that number, and
//! batch numbers never repeat (the open generation is in their high bits).
//! Every batch a recycled file still holds is older than the segment it
//! becomes, so its records and entries read as stale bytes, like never
//! written ones, and recovery's verdicts stay those of the crate docs.
//!
//! A spare holds no batch of its own: no batch is written to it before its
//! rename. An open keeps one whole spare it finds and deletes the others.
//! With no spare, a roll creates its segment with
//! [`allocate`](moonpool_core::StorageFile::allocate): the space is
//! reserved, reads as zeros, and nothing is written but the headers.

const PREFIX: &str = "spare-";
const SUFFIX: &str = ".wal";

/// `spare-0000000007-0000000003.wal`: the fourth spare of open generation
/// 7. Every spare has its own name.
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
