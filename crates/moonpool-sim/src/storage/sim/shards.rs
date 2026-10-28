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
use crate::runner::shard::{Requirements, replica_ip};
use crate::storage::faults::SECTOR_SIZE;

/// For one file: whether damage may land on each `(shard, stripe)` key of
/// its regions inside a shard.
#[derive(Debug, Default)]
pub(crate) struct ShardRule {
    keys: Vec<(ShardKey, bool)>,
}

/// A `(shard, stripe)` key.
type ShardKey = (Arc<str>, Option<u64>);

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
    /// The replicas whose copies count as lost whatever their bytes say:
    /// wiped and not healed, or on a failed disk.
    fn not_intact(&self) -> BTreeSet<IpAddr> {
        let mut out = self
            .shards
            .as_ref()
            .map(crate::runner::shard::ShardBook::wiped)
            .unwrap_or_default();
        out.extend(self.failed_disks.iter().copied());
        out
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
        not_intact: &BTreeSet<IpAddr>,
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
                        && !not_intact.contains(&ip)
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

    /// Whether a crash of `ip` now could break a shard's `durable` need: a
    /// dirty sector of one of its files lies in a region whose copy the
    /// shard cannot afford to lose.
    pub(crate) fn crash_endangers_shards(&self, ip: IpAddr) -> bool {
        let Some(layout) = self.layouts.get(&ip) else {
            return false;
        };
        let Some(requirements) = self.shard_requirements() else {
            return false;
        };
        let not_intact = self.not_intact();
        self.path_to_file.iter().any(|((owner, path), file_id)| {
            if *owner != ip {
                return false;
            }
            let Some(file) = self.files.get(file_id) else {
                return false;
            };
            file.image.dirty_sectors().into_iter().any(|sector| {
                layout.entries(path, sector).iter().any(|(stripe, shard)| {
                    shard.as_deref().is_some_and(|shard| {
                        !self.stripe_survives(&requirements, &not_intact, ip, shard, *stripe)
                    })
                })
            })
        })
    }

    /// Whether failing `ip`'s disk keeps every shard that lists it durable.
    pub(crate) fn disk_failure_allowed(&self, ip: IpAddr) -> bool {
        let Some(requirements) = self.shard_requirements() else {
            return true;
        };
        let not_intact = self.not_intact();
        crate::runner::shard::pure(|| {
            crate::runner::shard::lose_ok(&requirements, &not_intact, &[ip])
        })
    }
}
