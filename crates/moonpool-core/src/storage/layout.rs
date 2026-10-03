//! A storage format's on-disk layout, described for whoever needs to know
//! where its bytes live — a fault injector, first of all.

use std::ops::Range;

/// One named byte range of one file in a storage format's on-disk layout:
/// "the slot of entry 17 is bytes 9216..9280 of `wal/seg-…1.wal`".
///
/// A format that knows its layout lists its regions this way, and a
/// simulator can aim faults at them without knowing the format: the `kind`
/// is the format's own label for what the bytes hold (`"slot"`, `"entry"`,
/// `"header"`…), so a harness can weigh kinds differently. The optional
/// `stripe` says which *replicated* record the bytes hold, as a key every
/// replica's copy shares — a Paxos slot, a page number — so a simulator can
/// damage a record on some replicas and never on all of them, however
/// differently each replica laid it out. Nothing here depends on a
/// simulator; it is plain data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LayoutRegion {
    /// The file's path, as the format opens it.
    pub path: String,
    /// The byte range within the file.
    pub bytes: Range<u64>,
    /// The format's label for what these bytes hold.
    pub kind: &'static str,
    /// The replicated record these bytes hold, as a key shared by every
    /// replica's copy of it; `None` for bytes only this node has (headers,
    /// local metadata), whose loss costs the node rather than one record.
    pub stripe: Option<u64>,
}

impl LayoutRegion {
    /// This region, holding the replicated record `stripe` (or none).
    #[must_use]
    pub fn striped(mut self, stripe: Option<u64>) -> Self {
        self.stripe = stripe;
        self
    }

    /// Whether this region shares a byte with `bytes` of the file at `path`.
    #[must_use]
    pub fn overlaps(&self, path: &str, bytes: &Range<u64>) -> bool {
        self.path == path && self.bytes.start < bytes.end && bytes.start < self.bytes.end
    }
}
