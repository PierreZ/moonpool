//! A storage format's on-disk layout, described for whoever needs to know
//! where its bytes live — a fault injector, first of all.

use std::ops::Range;

/// One named byte range of one file in a storage format's on-disk layout:
/// "the slot of entry 17 is bytes 9216..9280 of `wal/seg-…1.wal`".
///
/// A format that knows its layout lists its regions this way, and a
/// simulator can aim faults at them without knowing the format: the `kind`
/// is the format's own label for what the bytes hold (`"slot"`, `"entry"`,
/// `"header"`…), so a harness can weigh kinds differently. Nothing here
/// depends on a simulator; it is plain data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LayoutRegion {
    /// The file's path, as the format opens it.
    pub path: String,
    /// The byte range within the file.
    pub bytes: Range<u64>,
    /// The format's label for what these bytes hold.
    pub kind: &'static str,
}

impl LayoutRegion {
    /// Whether this region shares a byte with `bytes` of the file at `path`.
    #[must_use]
    pub fn overlaps(&self, path: &str, bytes: &Range<u64>) -> bool {
        self.path == path && self.bytes.start < bytes.end && bytes.start < self.bytes.end
    }
}
