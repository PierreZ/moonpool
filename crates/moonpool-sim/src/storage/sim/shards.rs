//! The storage side of the shard gate: where damage may land, judged on
//! each shard's `durable` need over the copies still clean.
//!
//! A replica's copy of `(shard, stripe)` is the bytes it publishes for that
//! key with [`publish_layout`](crate::SimStorageProvider::publish_layout).
//! The copy is clean while the replica is not wiped (or healed), its disk
//! has not failed, and none of those bytes is damaged. A region of the
//! shard with no stripe holds bytes the whole copy depends on: damage there
//! costs the replica every stripe of the shard.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::ops::Range;
use std::sync::Arc;

use moonpool_buggify::shard::Replication;

use super::state::StorageState;
use crate::runner::shard::{Requirements, ShardLosses, replica_ip};
use crate::storage::faults::SECTOR_SIZE;

/// For one file: whether damage may land on each `(shard, stripe)` key of
/// its regions inside a shard.
#[derive(Debug, Default)]
pub(crate) struct ShardRule {
    keys: Vec<(ShardKey, bool)>,
}

/// A `(shard, stripe)` key.
type ShardKey = (Arc<str>, Option<u64>);

/// The copies lost whatever their bytes say ([`StorageState::not_intact`]).
struct NotIntact {
    /// Replicas whose copies of every shard are lost.
    everywhere: BTreeSet<IpAddr>,
    /// Per shard, the replicas whose copy of that shard alone is lost.
    per_shard: ShardLosses,
}

impl NotIntact {
    /// Whether `ip`'s copy of `shard` is lost.
    fn lost(&self, ip: IpAddr, shard: &str) -> bool {
        self.everywhere.contains(&ip)
            || self
                .per_shard
                .get(shard)
                .is_some_and(|lost| lost.contains(&ip))
    }
}

impl ShardRule {
    /// For a sector whose regions are `entries`: `None` when no region is
    /// inside a shard; else whether damage may land there, and whether
    /// every region is inside a shard (no replicated fault pattern applies).
    pub(crate) fn verdict(
        &self,
        entries: &[crate::storage::replication::Entry],
    ) -> Option<(bool, bool)> {
        let mut any = false;
        let mut safe = true;
        for (stripe, shard) in entries {
            let Some(shard) = shard else {
                continue;
            };
            any = true;
            let allowed = self
                .keys
                .iter()
                .find(|((name, key), _)| name == shard && key == stripe)
                .is_none_or(|(_, allowed)| *allowed);
            safe &= allowed;
        }
        let only = entries.iter().all(|(_, shard)| shard.is_some());
        any.then_some((safe, only))
    }
}

impl StorageState {
    /// The copies that count as lost whatever their bytes say: in every
    /// shard, the replicas wiped and not healed or on a failed disk; in one
    /// shard, the copies lost in that shard alone.
    fn not_intact(&self) -> NotIntact {
        let mut everywhere = self
            .shards
            .as_ref()
            .map(crate::runner::shard::ShardBook::wiped)
            .unwrap_or_default();
        everywhere.extend(self.failed_disks.iter().copied());
        let per_shard = self
            .shards
            .as_ref()
            .map(crate::runner::shard::ShardBook::losses)
            .unwrap_or_default();
        NotIntact {
            everywhere,
            per_shard,
        }
    }

    /// The shards the gate covers, if a book is installed and declares any.
    fn shard_requirements(&self) -> Option<Requirements> {
        let requirements = self.shards.as_ref()?.requirements();
        (!requirements.is_empty()).then_some(requirements)
    }

    /// Whether `ip`'s copy of `(shard, stripe)` holds a damaged sector: a
    /// region of that key, or a stripe-less region of the shard. With
    /// `stripe` `None`, any region of the shard counts.
    pub(crate) fn copy_damaged(&self, ip: IpAddr, shard: &str, stripe: Option<u64>) -> bool {
        let Some(layout) = self.layouts.get(&ip) else {
            return false;
        };
        let size = SECTOR_SIZE as u64;
        layout.sharded().any(|(path, bytes, name, key)| {
            if name != shard || (stripe.is_some() && key.is_some() && key != stripe) {
                return false;
            }
            let Some(file) = self
                .path_to_file
                .get(&(ip, path.to_string()))
                .and_then(|file_id| self.files.get(file_id))
            else {
                return false;
            };
            if !file.image.has_damage() {
                return false;
            }
            let sectors = bytes.start / size..bytes.end.div_ceil(size);
            file.image
                .damaged_sectors()
                .into_iter()
                .any(|sector| sectors.contains(&sector))
        })
    }

    /// Whether `ip`'s copy of `(shard, stripe)` is clean on its disk now:
    /// disk not failed and no damaged sector (wipes are the book's).
    pub(crate) fn copy_intact(&self, ip: IpAddr, shard: &str, stripe: Option<u64>) -> bool {
        !self.failed_disks.contains(&ip) && !self.copy_damaged(ip, shard, stripe)
    }

    /// Whether losing `owner`'s copy of `(shard, stripe)` keeps `durable` of
    /// every replication the shard requires, judged on the other copies
    /// still clean.
    fn stripe_survives(
        &self,
        requirements: &Requirements,
        not_intact: &NotIntact,
        owner: IpAddr,
        shard: &str,
        stripe: Option<u64>,
    ) -> bool {
        let Some((_, replications)) = requirements.iter().find(|(name, _)| name == shard) else {
            return true;
        };
        replications.iter().all(|replication: &Replication| {
            replication.durable.holds(&|replica| {
                replica_ip(replica).is_none_or(|ip| {
                    ip != owner
                        && !not_intact.lost(ip, shard)
                        && !self.copy_damaged(ip, shard, stripe)
                })
            })
        })
    }

    /// The placement rule for damage on `owner`'s file at `path`, when the
    /// file holds regions inside a declared shard.
    pub(crate) fn shard_rule(&self, owner: IpAddr, path: &str) -> Option<ShardRule> {
        let layout = self.layouts.get(&owner)?;
        let keys = layout.shard_keys(path);
        if keys.is_empty() {
            return None;
        }
        let requirements = self.shard_requirements()?;
        let not_intact = self.not_intact();
        let keys = keys
            .into_iter()
            .map(|(shard, stripe)| {
                let allowed =
                    self.stripe_survives(&requirements, &not_intact, owner, &shard, stripe);
                ((shard, stripe), allowed)
            })
            .collect();
        Some(ShardRule { keys })
    }

    /// Whether damage to `bytes` of `owner`'s file at `path` keeps every
    /// shard those bytes belong to durable.
    pub(crate) fn damage_allowed(&self, owner: IpAddr, path: &str, bytes: &Range<u64>) -> bool {
        let Some(layout) = self.layouts.get(&owner) else {
            return true;
        };
        let entries = layout.entries_in(path, bytes);
        if entries.iter().all(|(_, shard)| shard.is_none()) {
            return true;
        }
        let Some(requirements) = self.shard_requirements() else {
            return true;
        };
        let not_intact = self.not_intact();
        entries.iter().all(|(stripe, shard)| {
            shard.as_deref().is_none_or(|shard| {
                self.stripe_survives(&requirements, &not_intact, owner, shard, *stripe)
            })
        })
    }

    /// Whether `owner` may lose every copy it holds (a wipe, or damage its
    /// format answers by giving up the whole replica) and every shard keep
    /// its `durable` need: on the replicas left intact, and per stripe on
    /// the copies left clean.
    pub(crate) fn replica_loss_allowed(&self, owner: IpAddr) -> bool {
        let Some(requirements) = self.shard_requirements() else {
            return true;
        };
        let not_intact = self.not_intact();
        if !crate::runner::shard::lose_ok(
            &requirements,
            &not_intact.everywhere,
            &not_intact.per_shard,
            &[owner],
        ) {
            return false;
        }
        self.layouts
            .iter()
            .filter(|(ip, _)| **ip != owner)
            .all(|(ip, layout)| {
                layout.sharded().all(|(_, _, shard, stripe)| {
                    !self.copy_damaged(*ip, shard, stripe)
                        || self.stripe_survives(&requirements, &not_intact, owner, shard, stripe)
                })
            })
    }

    /// Whether a crash of `ip` now could break a shard's `durable` need: a
    /// dirty sector of one of its files lies in a region whose copy the
    /// shard cannot afford to lose.
    pub(crate) fn crash_endangers_shards(&self, ip: IpAddr) -> bool {
        if !self.layouts.contains_key(&ip) {
            return false;
        }
        let Some(requirements) = self.shard_requirements() else {
            return false;
        };
        let mut not_intact = self.not_intact();
        // A crash decided for another replica and not landed yet may tear
        // that replica's dirty whole copies: judge as if it did.
        let pending = self
            .shards
            .as_ref()
            .map(crate::runner::shard::ShardBook::pending_crashes)
            .unwrap_or_default();
        for other in pending.into_iter().filter(|other| *other != ip) {
            for shard in self.dirty_whole_copies(other) {
                not_intact.per_shard.entry(shard).or_default().insert(other);
            }
        }
        self.dirty_whole_copies(ip)
            .iter()
            .any(|shard| !self.stripe_survives(&requirements, &not_intact, ip, shard, None))
    }

    /// The shards of which `ip` holds a whole copy (no striped region) with
    /// a dirty sector: the copies a crash of `ip` can tear. A dirty sector
    /// of a striped region is left out: while its stripe cannot afford
    /// damage, the crash model keeps it old or new, which costs no copy.
    fn dirty_whole_copies(&self, ip: IpAddr) -> BTreeSet<String> {
        let Some(layout) = self.layouts.get(&ip) else {
            return BTreeSet::new();
        };
        let mut shards = BTreeSet::new();
        for ((owner, path), file_id) in &self.path_to_file {
            if *owner != ip {
                continue;
            }
            let Some(file) = self.files.get(file_id) else {
                continue;
            };
            for sector in file.image.dirty_sectors() {
                for (stripe, shard) in layout.entries(path, sector) {
                    if let (None, Some(shard)) = (stripe, shard)
                        && !layout
                            .sharded()
                            .any(|(_, _, name, stripe)| name == &*shard && stripe.is_some())
                    {
                        shards.insert(shard.to_string());
                    }
                }
            }
        }
        shards
    }

    /// Count `owner`'s copy of a shard lost when a crash changed a dirty
    /// sector of a copy with no striped region in that shard: such a copy
    /// stands or falls whole, and a format may refuse it whole at its next open (a
    /// registry whose last batch was torn). The copy counts lost until the
    /// code under test says it caught up
    /// ([`moonpool_buggify::shard::caught_up`]), at an open that found it
    /// intact. A copy the format refuses stays lost.
    pub(crate) fn note_torn_whole_copies(
        &self,
        owner: IpAddr,
        path: &str,
        report: &crate::storage::faults::FileCrashReport,
    ) {
        let (Some(book), Some(layout)) = (self.shards.as_ref(), self.layouts.get(&owner)) else {
            return;
        };
        for resolution in &report.resolutions {
            if resolution.outcome == crate::storage::faults::CrashOutcome::KeptNew {
                continue;
            }
            for (stripe, shard) in layout.entries(path, resolution.sector) {
                // Only a copy with no striped region at all is whole: a
                // stripe-less region beside striped ones (a journal's
                // metainfo, kept twice) is not the copy itself.
                if let (None, Some(shard)) = (stripe, shard)
                    && !layout
                        .sharded()
                        .any(|(_, _, name, stripe)| name == &*shard && stripe.is_some())
                {
                    book.note_shard_loss(owner, &shard);
                }
            }
        }
    }

    /// Whether a crash may lose the unsynced directory entries at `paths` on
    /// `owner`'s disk: a lost entry costs every `(shard, stripe)` copy its
    /// file holds, so each must survive on the other clean copies.
    pub(crate) fn entry_loss_allowed(&self, owner: IpAddr, paths: &[&str]) -> bool {
        let Some(layout) = self.layouts.get(&owner) else {
            return true;
        };
        let keys: Vec<ShardKey> = paths
            .iter()
            .flat_map(|path| layout.shard_keys(path))
            .collect();
        if keys.is_empty() {
            return true;
        }
        let Some(requirements) = self.shard_requirements() else {
            return true;
        };
        let not_intact = self.not_intact();
        keys.iter().all(|(shard, stripe)| {
            self.stripe_survives(&requirements, &not_intact, owner, shard, *stripe)
        })
    }

    /// Count `owner`'s copies at `paths` lost after a crash lost their
    /// directory entries: the file is gone or back to an older one, so each
    /// shard it held counts lost until the code under test opens it intact
    /// ([`moonpool_buggify::shard::intact`]).
    pub(crate) fn note_lost_entries(&self, owner: IpAddr, paths: &[&str]) {
        let (Some(book), Some(layout)) = (self.shards.as_ref(), self.layouts.get(&owner)) else {
            return;
        };
        let shards: BTreeSet<Arc<str>> = paths
            .iter()
            .flat_map(|path| layout.shard_keys(path))
            .map(|(shard, _)| shard)
            .collect();
        for shard in shards {
            book.note_shard_loss(owner, &shard);
        }
    }

    /// Whether failing `ip`'s disk keeps every shard that lists it durable.
    pub(crate) fn disk_failure_allowed(&self, ip: IpAddr) -> bool {
        let Some(requirements) = self.shard_requirements() else {
            return true;
        };
        let not_intact = self.not_intact();
        crate::runner::shard::pure(|| {
            crate::runner::shard::lose_ok(
                &requirements,
                &not_intact.everywhere,
                &not_intact.per_shard,
                &[ip],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, net::IpAddr, task::Poll, time::Duration};

    use moonpool_buggify::shard::{Declaration, Replica, Replication};
    use moonpool_core::{LayoutRegion, OpenOptions, StorageFile, StorageProvider};

    use crate::runner::shard::{ShardBook, ShardFaults};
    use crate::{SimWorld, StorageConfiguration};

    const A: &str = "10.0.1.1";
    const B: &str = "10.0.1.2";
    const C: &str = "10.0.1.3";

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP")
    }

    async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
        futures::pin_mut!(future);
        futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending if sim.has_pending_events() => {
                sim.step();
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        })
        .await
    }

    /// A world whose crashes lose every unsynced sector, and a book with one
    /// shard `s` on A, B and C that must keep two copies.
    fn world() -> (SimWorld, ShardBook) {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration {
            clean_crash_probability: 0.0,
            correlated_rollback_probability: 0.0,
            crash_lost_probability: 1.0,
            length_survives_crash_probability: 1.0,
            ..StorageConfiguration::fast_local()
        });
        let book = ShardBook::new(ShardFaults::default());
        sim.set_shard_book(Some(book.clone()));
        let replicas = [A, B, C].into_iter().map(Replica::new).collect();
        book.declare(
            Some(ip(A)),
            Duration::ZERO,
            Declaration {
                name: "s".to_string(),
                current: Some(Replication::tolerates(replicas, 1, 1)),
                next: None,
                site: "test:1",
            },
            || true,
        );
        (sim, book)
    }

    /// On `owner`: a synced file `path` with one region of shard `s` (striped
    /// or a whole copy), then an unsynced write into it.
    fn dirty_copy(sim: &mut SimWorld, owner: &str, path: &'static str, stripe: Option<u64>) {
        sim.publish_layout(
            ip(owner),
            &[LayoutRegion {
                path: path.to_string(),
                bytes: 0..4096,
                kind: "copy",
                stripe,
                shard: Some("s".to_string()),
            }],
        );
        write_unsynced(sim, owner, path);
    }

    fn write_unsynced(sim: &mut SimWorld, owner: &str, path: &'static str) {
        let provider = sim.storage_provider(ip(owner));
        crate::executor::Executor::new(0).block_on(drive(sim, async move {
            let file = provider
                .open(path, OpenOptions::create_write())
                .await
                .expect("open");
            file.write_at(0, &[1; 4096]).await.expect("write");
            file.sync_all().await.expect("sync");
            provider.sync_dir(".").await.expect("sync dir");
            file.write_at(0, &[2; 512]).await.expect("unsynced write");
        }));
    }

    #[test]
    fn a_domain_reboot_cannot_tear_two_whole_copies() {
        let (mut sim, book) = world();
        dirty_copy(&mut sim, A, "copy", None);
        dirty_copy(&mut sim, B, "copy", None);
        dirty_copy(&mut sim, C, "copy", None);
        assert!(
            !sim.crash_endangers_shards(ip(A)),
            "one torn copy leaves two"
        );
        // The gate let A's crash through; it lands later.
        book.note_pending_crash(ip(A));
        assert!(
            sim.crash_endangers_shards(ip(B)),
            "B's crash, judged with A's pending, would leave one copy"
        );
        sim.simulate_crash_for_process(ip(A), true);
        // A's crash landed and tore its copy for good.
        assert!(sim.crash_endangers_shards(ip(B)), "A's copy is lost now");
    }

    #[test]
    fn a_crash_on_a_degraded_striped_shard_still_rolls_back_unsynced_sectors() {
        let (mut sim, book) = world();
        dirty_copy(&mut sim, A, "striped", Some(7));
        // The shard is degraded: B lost its copies, so A's stripe cannot
        // afford damage.
        book.note_wipe(ip(B));
        write_unsynced(&mut sim, A, "unrelated");
        assert!(
            !sim.crash_endangers_shards(ip(A)),
            "a striped region needs no kept cache: the crash model guards it"
        );
        sim.simulate_crash_for_process(ip(A), true);
        let reports = sim.take_storage_crash_reports();
        let unrelated = reports
            .iter()
            .find(|report| report.path == "unrelated")
            .expect("a report for the unrelated file");
        assert!(
            unrelated
                .resolutions
                .iter()
                .any(|resolution| resolution.outcome == crate::CrashOutcome::Lost),
            "the unrelated file's unsynced sector rolled back: {unrelated:?}"
        );
        let striped = reports
            .iter()
            .find(|report| report.path == "striped")
            .expect("a report for the striped file");
        assert!(
            striped.resolutions.iter().all(|resolution| matches!(
                resolution.outcome,
                crate::CrashOutcome::KeptOld | crate::CrashOutcome::KeptNew
            )),
            "the guarded stripe kept old or new bytes: {striped:?}"
        );
    }
}
