//! The journal atlas: where every region recovery reads lives, and faults aimed by
//! it. The scan of a closed journal agrees with the open journal's own chart,
//! each entry's damage is reported at the index the atlas names, and a
//! randomized loop aims damage where recovery has to decide — the last batch,
//! an entry beside its own slot, both copies of a header or of the metadata —
//! and checks the verdict the regions predict.

use std::future::Future;
use std::net::IpAddr;

use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_journal::{
    AmbiguousTail, EntryId, Geometry, Journal, JournalAtlas, JournalConfig, JournalError,
    JournalRegion, Record, Recovery, TAG_SIZE, Tag,
};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

const DIR: &str = "wal";

fn ip() -> IpAddr {
    "10.0.0.1".parse().expect("valid IP")
}

/// A tiny geometry — 16 slots, 32 KiB segments — so a few batches roll
/// over, and a replicated caller's policy: ambiguous entries are kept.
fn config() -> JournalConfig {
    JournalConfig {
        geometry: Geometry {
            slot_count: 16,
            data_start: 12 * 1024,
            segment_size: 32 * 1024,
        },
        ambiguous_tail: AmbiguousTail::Keep,
        ..JournalConfig::default()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

fn sim(seed: u64) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Run `f` against `sim`'s storage, stepping the world until it finishes.
async fn run<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::spawn(f(sim.storage_provider(ip())));
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

fn tag(index: u64) -> Tag {
    let mut tag = [0; TAG_SIZE];
    tag[..8].copy_from_slice(&index.to_le_bytes());
    tag[8..16].copy_from_slice(&index.rotate_left(17).to_le_bytes());
    tag
}

/// A tiny deterministic generator for the test's own choices.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        items.get(usize::try_from(self.below(items.len() as u64)).expect("small"))
    }
}

/// Write a journal of `batches` batches of 1–4 entries each, with payloads
/// of 0–600 bytes, and maybe metadata; return the open journal's atlas.
async fn write_journal(provider: SimStorageProvider, seed: u64, batches: u64) -> JournalAtlas {
    let mut rng = Rng::new(seed);
    let (mut journal, _) = Journal::open(provider, DIR, config()).await.expect("open");
    if rng.chance(70) {
        journal.save_meta(b"term=1").await.expect("meta");
    }
    for _ in 0..batches {
        let count = 1 + rng.below(4);
        let first = journal.next_index();
        let payloads: Vec<Vec<u8>> = (0..count)
            .map(|k| {
                let len = usize::try_from(rng.below(600)).expect("small");
                vec![u8::try_from((first + k) % 251).expect("byte"); len]
            })
            .collect();
        let records: Vec<Record<'_>> = payloads
            .iter()
            .enumerate()
            .map(|(k, bytes)| Record::new(3, bytes).with_tag(tag(first + k as u64)))
            .collect();
        journal.append(&records).await.expect("append");
    }
    if rng.chance(50) {
        journal.save_meta(b"term=2,vote=9").await.expect("meta");
    }
    journal.atlas()
}

async fn scan(provider: SimStorageProvider) -> JournalAtlas {
    JournalAtlas::scan(&provider, DIR, config().geometry)
        .await
        .expect("scan")
}

async fn reopen(provider: SimStorageProvider) -> Result<Recovery, JournalError> {
    Journal::open(provider, DIR, config())
        .await
        .map(|(_, recovery)| recovery)
}

/// What one injection does to a region's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// Flip one bit: the region fails its checksum.
    Flip,
    /// Zero the whole region: a lost write. A zeroed slot reads as empty —
    /// no identifier — rather than damaged.
    Zero,
}

/// Damage `region` as `damage` says, at a byte the generator picks.
async fn inflict(
    provider: SimStorageProvider,
    atlas: JournalAtlas,
    hits: Vec<(JournalRegion, Damage, u64)>,
) -> std::io::Result<()> {
    for (region, damage, pick) in hits {
        let layout = atlas.locate(region).expect("charted region").clone();
        let file = provider
            .open(&layout.path, OpenOptions::read_write())
            .await?;
        match damage {
            Damage::Flip => {
                let at = layout.bytes.start + pick % (layout.bytes.end - layout.bytes.start);
                let mut byte = [0u8; 1];
                file.read_at(at, &mut byte).await?;
                byte[0] ^= 0x04;
                file.write_at(at, &byte).await?;
            }
            Damage::Zero => {
                let len = layout.bytes.end - layout.bytes.start;
                let zeros = vec![0u8; usize::try_from(len).expect("small")];
                file.write_at(layout.bytes.start, &zeros).await?;
            }
        }
        file.sync_all().await?;
    }
    Ok(())
}

/// What reopening must report, derived from the regions alone.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Refused(String),
    Opened {
        corrupt: Vec<EntryId>,
        ambiguous: Vec<EntryId>,
        slots_rewritten: usize,
        torn_tail: bool,
        headers_repaired: usize,
        meta_repaired: bool,
    },
}

fn verdict_of(recovery: &Result<Recovery, JournalError>) -> Verdict {
    match recovery {
        Err(JournalError::MetadataCorrupt { .. }) => Verdict::Refused("metadata".into()),
        Err(JournalError::BadSegmentHeader { first_index }) => {
            Verdict::Refused(format!("header {first_index}"))
        }
        Err(JournalError::DoubleFault { index }) => Verdict::Refused(format!("double {index}")),
        Err(other) => Verdict::Refused(format!("unexpected {other}")),
        Ok(recovery) => Verdict::Opened {
            corrupt: recovery.corrupt.clone(),
            ambiguous: recovery.ambiguous_batch.clone(),
            slots_rewritten: recovery.slots_rewritten,
            torn_tail: recovery.torn_tail,
            headers_repaired: recovery.headers_repaired,
            meta_repaired: recovery.meta_repaired,
        },
    }
}

/// The recovery table applied to the damaged regions: metadata first, then
/// segment by segment, its headers and then its indexes in order.
fn predict(atlas: &JournalAtlas, hits: &[(JournalRegion, Damage, u64)]) -> Verdict {
    let damage = |region: JournalRegion| {
        hits.iter()
            .find(|(hit, ..)| *hit == region)
            .map(|(_, damage, _)| *damage)
    };
    let metas: Vec<JournalRegion> = atlas
        .regions()
        .iter()
        .map(|located| located.region)
        .filter(|region| matches!(region, JournalRegion::Meta { .. }))
        .collect();
    let bad_metas = metas.iter().filter(|r| damage(**r).is_some()).count();
    if !metas.is_empty() && bad_metas == metas.len() {
        return Verdict::Refused("metadata".into());
    }
    let mut headers_repaired = 0;
    let mut slots_rewritten = 0;
    let mut corrupt = Vec::new();
    let mut survivors = Vec::new();
    let mut torn_tail = false;
    let segments: Vec<u64> = atlas
        .regions()
        .iter()
        .filter_map(|located| match located.region {
            JournalRegion::Header { segment, copy: 0 } => Some(segment),
            _ => None,
        })
        .collect();
    let live = atlas.live();
    'segments: for (k, first) in segments.iter().enumerate() {
        let bad_headers = (0..2u8)
            .filter(|copy| {
                damage(JournalRegion::Header {
                    segment: *first,
                    copy: *copy,
                })
                .is_some()
            })
            .count();
        if bad_headers == 2 {
            return Verdict::Refused(format!("header {first}"));
        }
        headers_repaired += bad_headers;
        let end = segments.get(k + 1).copied().unwrap_or(live.end);
        for index in *first..end.max(*first) {
            let Some(entry) = atlas.entry(index) else {
                continue;
            };
            let JournalRegion::Entry(id) = entry.region else {
                unreachable!("an entry region")
            };
            let entry_bad = damage(JournalRegion::Entry(id)).is_some();
            match (entry_bad, damage(JournalRegion::Slot(id))) {
                (false, None) => survivors.push(id),
                (false, Some(_)) => {
                    slots_rewritten += 1;
                    survivors.push(id);
                }
                (true, None) => {
                    corrupt.push(id);
                    survivors.push(id);
                }
                (true, Some(_)) => {
                    // No entry and no identifier — zeros are damage like any
                    // other. In the last batch (the tail segment, no later
                    // batch start among the intact identifiers) a crash can
                    // tear both, and the log ends here; anywhere before, it
                    // is a double fault.
                    let later_start = (index + 1..end).any(|later| {
                        atlas.starts_batch(later)
                            && atlas.entry(later).is_some_and(|e| {
                                let JournalRegion::Entry(id) = e.region else {
                                    unreachable!("an entry region")
                                };
                                damage(JournalRegion::Slot(id)).is_none()
                            })
                    });
                    if k + 1 < segments.len() || later_start {
                        return Verdict::Refused(format!("double {index}"));
                    }
                    torn_tail = true;
                    break 'segments;
                }
            }
        }
    }
    // The survivors' last batch: from the last batch start to the end.
    let batch_from = survivors
        .iter()
        .rev()
        .find(|id| atlas.starts_batch(id.index))
        .map_or(u64::MAX, |id| id.index);
    let (ambiguous, corrupt): (Vec<EntryId>, Vec<EntryId>) =
        corrupt.into_iter().partition(|id| id.index >= batch_from);
    Verdict::Opened {
        corrupt,
        ambiguous,
        slots_rewritten,
        torn_tail,
        headers_repaired,
        meta_repaired: bad_metas == 1,
    }
}

/// How far the randomized loop goes: `JOURNAL_ATLAS_SEEDS` raises it.
fn seed_count() -> u64 {
    std::env::var("JOURNAL_ATLAS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150)
}

/// The scan of the closed directory charts what the open journal charted.
#[test]
fn a_scan_of_the_closed_journal_matches_the_open_journals_atlas() {
    runtime().block_on(async {
        for seed in 0..40 {
            let mut sim = sim(seed);
            let batches = 1 + seed % 12;
            let charted = run(&mut sim, move |p| write_journal(p, seed, batches)).await;
            let scanned = run(&mut sim, scan).await;
            assert_eq!(scanned, charted, "seed {seed}");
            assert!(!charted.last_batch().is_empty(), "seed {seed}");
        }
    });
}

/// moonpool#289's acceptance: damaging each charted entry gets it reported
/// at its index — corrupt, or ambiguous in the last batch — and damaging it
/// with its slot gets a double fault there, or, in the last batch, a torn
/// tail that ends the log there.
#[test]
fn each_charted_entry_is_reported_where_the_atlas_says() {
    runtime().block_on(async {
        let seed = 3;
        let mut probe = sim(seed);
        let atlas = run(&mut probe, move |p| write_journal(p, seed, 7)).await;
        assert!(atlas.live().end - atlas.live().start > 8, "spans segments");
        for index in atlas.live() {
            let entry = atlas.entry(index).expect("charted").region;
            let JournalRegion::Entry(id) = entry else {
                unreachable!("an entry region")
            };
            let mut sim = sim(seed);
            run(&mut sim, move |p| write_journal(p, seed, 7)).await;
            let hit = vec![(entry, Damage::Flip, index)];
            let atlas2 = atlas.clone();
            run(&mut sim, move |p| inflict(p, atlas2, hit))
                .await
                .expect("inflict");
            let recovery = run(&mut sim, reopen).await.expect("reopen");
            if atlas.last_batch().contains(&index) {
                assert_eq!(recovery.ambiguous_batch, vec![id], "index {index}");
                assert!(recovery.corrupt.is_empty(), "index {index}");
            } else {
                assert_eq!(recovery.corrupt, vec![id], "index {index}");
                assert!(recovery.ambiguous_batch.is_empty(), "index {index}");
            }

            let mut sim = self::sim(seed);
            run(&mut sim, move |p| write_journal(p, seed, 7)).await;
            let hits = vec![
                (entry, Damage::Flip, index),
                (JournalRegion::Slot(id), Damage::Flip, index),
            ];
            let atlas2 = atlas.clone();
            run(&mut sim, move |p| inflict(p, atlas2, hits))
                .await
                .expect("inflict");
            let opened = run(&mut sim, reopen).await;
            if atlas.last_batch().contains(&index) {
                // A crash can tear both copies of an identity in the last
                // batch: the log ends there.
                let recovery = opened.expect("a torn last batch opens");
                assert!(recovery.torn_tail, "index {index}: {recovery:?}");
            } else {
                assert!(
                    matches!(opened, Err(JournalError::DoubleFault { index: at }) if at == index),
                    "index {index}: {opened:?}"
                );
            }
        }
    });
}

/// Choose this seed's injections from the atlas, biased toward where
/// recovery decides: the last batch, an entry beside its own slot, both
/// copies of a header or of the metadata, and a few independent picks.
fn aim(atlas: &JournalAtlas, rng: &mut Rng) -> Vec<(JournalRegion, Damage, u64)> {
    let mut hits: Vec<(JournalRegion, Damage)> = Vec::new();
    let regions: Vec<JournalRegion> = atlas.regions().iter().map(|l| l.region).collect();
    let of = |want: fn(&JournalRegion) -> bool| -> Vec<JournalRegion> {
        regions.iter().copied().filter(want).collect()
    };
    let entries = of(|r| matches!(r, JournalRegion::Entry(_)));
    let last: Vec<JournalRegion> = entries
        .iter()
        .copied()
        .filter(|r| matches!(r, JournalRegion::Entry(id) if atlas.last_batch().contains(&id.index)))
        .collect();
    let damage = |rng: &mut Rng| {
        if rng.chance(70) {
            Damage::Flip
        } else {
            Damage::Zero
        }
    };
    match rng.below(6) {
        0 => {
            // Part or all of the last batch.
            for region in &last {
                if rng.chance(60) {
                    hits.push((*region, damage(rng)));
                }
            }
        }
        1 => {
            // An entry and its own identifier.
            if let Some(JournalRegion::Entry(id)) = rng.pick(&entries).copied() {
                hits.push((JournalRegion::Entry(id), damage(rng)));
                hits.push((JournalRegion::Slot(id), damage(rng)));
            }
        }
        2 => {
            // Both copies of a twin: a header pair or the metadata pair.
            let twins = of(|r| {
                matches!(
                    r,
                    JournalRegion::Header { copy: 0, .. } | JournalRegion::Meta { copy: 0 }
                )
            });
            if let Some(region) = rng.pick(&twins).copied() {
                let other = match region {
                    JournalRegion::Header { segment, .. } => {
                        JournalRegion::Header { segment, copy: 1 }
                    }
                    _ => JournalRegion::Meta { copy: 1 },
                };
                hits.push((region, damage(rng)));
                if rng.chance(50) {
                    hits.push((other, damage(rng)));
                }
            }
        }
        3 => {
            // A run of slots, as one damaged sector of the table would.
            if let Some(JournalRegion::Entry(id)) = rng.pick(&entries).copied() {
                for index in id.index..id.index + 1 + rng.below(8) {
                    if let Some(slot) = atlas.slot(index) {
                        hits.push((slot.region, damage(rng)));
                    }
                }
            }
        }
        _ => {
            // Independent picks anywhere recovery reads.
            for _ in 0..=rng.below(3) {
                if let Some(region) = rng.pick(&regions).copied() {
                    hits.push((region, damage(rng)));
                }
            }
        }
    }
    let mut aimed: Vec<(JournalRegion, Damage, u64)> = Vec::new();
    for (region, damage) in hits {
        if !aimed.iter().any(|(seen, ..)| *seen == region) {
            aimed.push((region, damage, rng.next()));
        }
    }
    aimed
}

/// Faults aimed by the atlas, judged by the regions they hit: every reopen
/// reports exactly what the recovery table predicts, and a second reopen
/// finds every repair durable.
#[test]
fn faults_aimed_by_the_atlas_get_the_verdict_their_regions_predict() {
    runtime().block_on(async {
        let mut refused = 0;
        let mut ambiguous = 0;
        let mut double = 0;
        for seed in 0..seed_count() {
            let mut sim = sim(seed);
            let mut rng = Rng::new(seed ^ 0xA71A5);
            let batches = 1 + rng.below(14);
            let atlas = run(&mut sim, move |p| write_journal(p, seed, batches)).await;
            let hits = aim(&atlas, &mut rng);
            let expected = predict(&atlas, &hits);
            let (atlas2, hits2) = (atlas.clone(), hits.clone());
            run(&mut sim, move |p| inflict(p, atlas2, hits2))
                .await
                .expect("inflict");
            let recovery = run(&mut sim, reopen).await;
            let got = verdict_of(&recovery);
            assert_eq!(got, expected, "seed {seed}: hits {hits:?}");
            match &got {
                Verdict::Refused(why) => {
                    refused += 1;
                    double += usize::from(why.starts_with("double"));
                }
                Verdict::Opened { ambiguous: a, .. } => ambiguous += usize::from(!a.is_empty()),
            }
            if let Verdict::Opened {
                corrupt, ambiguous, ..
            } = got
            {
                let again = run(&mut sim, reopen).await.expect("second reopen");
                assert_eq!(
                    again,
                    Recovery {
                        corrupt,
                        ambiguous_batch: ambiguous,
                        ..Recovery::default()
                    },
                    "seed {seed}: every repair is durable"
                );
            }
        }
        eprintln!("refused {refused} (double faults {double}), ambiguous batches {ambiguous}");
        assert!(
            refused > 0 && double > 0 && ambiguous > 0,
            "the aim reaches every verdict"
        );
    });
}
