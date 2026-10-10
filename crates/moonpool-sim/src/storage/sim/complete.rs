//! Operation completion: reads, writes, syncs and set-length, with the faults each one can land.

use std::ops::Range;

use super::engine::{StorageActions, StorageCompletion, StorageEngine, misdirected_read_offset};
use super::{
    FileId, HandleId, OperationId,
    state::{PendingStorageOp, StorageState},
};
use crate::{
    assert_reachable,
    chaos::fault_events::SimFaultEvent,
    sim::rng::{sim_random, sim_random_range},
    storage::{
        StorageConfiguration, StorageError, StorageFaultKind,
        faults::{sector_range, weighted},
    },
};

/// What the disk decided to do with one write.
#[derive(Debug, Clone, Copy)]
enum WriteLanding {
    /// The device refused it.
    Eio,
    /// It was acknowledged and the bytes never reached the disk.
    Phantom,
    /// It landed at this offset — the requested one, or another entirely.
    At(u64),
}

impl StorageEngine {
    pub(super) fn complete_read(
        &mut self,
        pending: &PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<StorageCompletion, StorageError> {
        let (owner_ip, path, file_size, config) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| {
                (
                    file.owner_ip,
                    file.path.clone(),
                    file.image.size(),
                    self.state.config_for(file.owner_ip).clone(),
                )
            })
            .ok_or(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            })?;
        // The length was clamped to the end of file when the read was
        // submitted, but another handle may have truncated the file while it
        // was in flight. Reconcile with the end of file *now*: the bytes that
        // are gone read as a short read (or EOF), exactly as they would on a
        // real file, never as an error no injected fault explains.
        let len = usize::try_from(file_size.saturating_sub(pending.offset))
            .unwrap_or(usize::MAX)
            .min(pending.len);
        if len < pending.len {
            assert_reachable!("disk: in-flight read shortened by a truncation");
        }
        if len == 0 {
            return Ok(StorageCompletion::Read(Vec::new()));
        }
        self.state.settle_turns(owner_ip);
        let sectors = sector_range(pending.offset, len);

        // EIO: a targeted injection fires unconditionally; the random family
        // is rolled first, then gated by the eligibility mask, so installing a
        // mask never shifts the stream.
        let eio_roll = sim_random::<f64>();
        let targeted = self
            .state
            .files
            .get(&pending.file_id)
            .is_some_and(|file| file.image.read_fails(sectors.clone()));
        let random_hit = config.read_eio_probability > 0.0
            && eio_roll
                < weighted(
                    config.read_eio_probability,
                    self.state.weight_range(owner_ip, &path, sectors.clone()),
                );
        if targeted || random_hit {
            assert_reachable!("disk fault: read failed with EIO");
            self.record(&path, StorageFaultKind::EioRead, Some(sectors));
            actions.fault(SimFaultEvent::StorageReadFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
            return Err(StorageError::Io {
                file_id: pending.file_id,
                kind: std::io::ErrorKind::Other,
                message: "read failed (simulated I/O error)".to_string(),
            });
        }

        // Read-time latent corruption: the sector is damaged from now on, and
        // damaged identically on every retry.
        let mut read_faulted = false;
        if config.read_corruption_probability > 0.0 {
            for sector in sectors.clone() {
                let roll = sim_random::<f64>();
                let weight = self.state.weight(owner_ip, &path, sector);
                if roll < weighted(config.read_corruption_probability, weight) {
                    self.plant_latent(pending.file_id, sector);
                    read_faulted = true;
                }
            }
        }
        if read_faulted {
            assert_reachable!("disk fault: read planted latent corruption");
            self.record(&path, StorageFaultKind::ReadCorruption, Some(sectors));
        }

        let read_offset = misdirected_read_offset(&config, pending.offset, len, file_size);
        let misdirected = read_offset != pending.offset;
        if misdirected {
            assert_reachable!("disk fault: read served from the wrong offset");
            self.record(
                &path,
                StorageFaultKind::MisdirectedRead,
                Some(sector_range(read_offset, len)),
            );
        }

        let mut data = vec![0; len];
        let file =
            self.state
                .files
                .get(&pending.file_id)
                .ok_or(StorageError::InvalidFileHandle {
                    handle_id: pending.handle_id,
                })?;
        file.image
            .read(read_offset, &mut data)
            .map_err(|error| StorageError::Io {
                file_id: pending.file_id,
                kind: error.kind(),
                message: error.to_string(),
            })?;

        if read_faulted || misdirected {
            actions.fault(SimFaultEvent::StorageReadFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
        }
        Ok(StorageCompletion::Read(data))
    }

    /// Where one write actually lands, once the disk has had its say.
    pub(super) fn complete_write(
        &mut self,
        operation_id: OperationId,
        pending: PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<StorageCompletion, StorageError> {
        let Some(data) = pending.data else {
            return Err(StorageError::InvalidOperationData { operation_id });
        };
        let (owner_ip, path, config, file_size) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| {
                (
                    file.owner_ip,
                    file.path.clone(),
                    self.state.config_for(file.owner_ip).clone(),
                    file.image.size(),
                )
            })
            .ok_or(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            })?;
        // Append mode is the stream cursor's business: a positioned write was
        // already told exactly where it goes.
        let offset = if pending.append {
            file_size
        } else {
            pending.offset
        };

        let landing =
            self.decide_write_landing(pending.file_id, offset, data.len(), file_size, &config);
        let mut fault_kind = None;
        match landing {
            WriteLanding::Eio => {
                self.record(
                    &path,
                    StorageFaultKind::EioWrite,
                    Some(sector_range(offset, data.len())),
                );
                actions.fault(SimFaultEvent::StorageWriteFault {
                    ip: owner_ip.to_string(),
                    file_id: pending.file_id.0,
                    write_kind: "eio".to_string(),
                });
                return Err(StorageError::Io {
                    file_id: pending.file_id,
                    kind: std::io::ErrorKind::Other,
                    message: "write failed (simulated I/O error)".to_string(),
                });
            }
            WriteLanding::Phantom => {
                // Acknowledged, never applied: the bytes never reach the disk
                // and a reader keeps seeing the old contents.
                self.record(
                    &path,
                    StorageFaultKind::PhantomWrite,
                    Some(sector_range(offset, data.len())),
                );
                self.mark_damaged(pending.file_id, sector_range(offset, data.len()));
                fault_kind = Some("phantom");
            }
            WriteLanding::At(landed_at) => {
                if landed_at != offset {
                    self.record(
                        &path,
                        StorageFaultKind::MisdirectedWrite,
                        Some(sector_range(landed_at, data.len())),
                    );
                    fault_kind = Some("misdirected");
                }
                self.land_write(pending.file_id, pending.handle_id, landed_at, offset, &data)?;
                if self.plant_write_corruption(
                    pending.file_id,
                    &path,
                    landed_at,
                    data.len(),
                    &config,
                ) {
                    self.record(
                        &path,
                        StorageFaultKind::WriteCorruption,
                        Some(sector_range(landed_at, data.len())),
                    );
                    fault_kind = Some("corruption");
                }
            }
        }

        if let Some(write_kind) = fault_kind {
            actions.fault(SimFaultEvent::StorageWriteFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
                write_kind: write_kind.to_string(),
            });
        }
        let len = data.len();
        if !pending.positioned
            && let Some(handle) = self.state.handles.get_mut(&pending.handle_id)
        {
            handle.position = offset + len as u64;
        }
        Ok(StorageCompletion::Write { offset, len })
    }

    /// Decide what the disk does with one write: refuse it, swallow it, or
    /// land it — here, or somewhere else entirely.
    ///
    /// Every roll happens before the eligibility mask is consulted, so
    /// installing a mask never shifts the random stream.
    fn decide_write_landing(
        &mut self,
        file_id: FileId,
        offset: u64,
        len: usize,
        file_size: u64,
        config: &StorageConfiguration,
    ) -> WriteLanding {
        let sectors = sector_range(offset, len);
        let eio_roll = sim_random::<f64>();
        let (targeted, path, owner) = self.state.files.get(&file_id).map_or_else(
            || (false, String::new(), None),
            |file| {
                (
                    file.image.write_fails(sectors.clone()),
                    file.path.clone(),
                    Some(file.owner_ip),
                )
            },
        );
        if let Some(owner) = owner {
            self.state.settle_turns(owner);
        }
        let path = path.as_str();
        let weigh_range = |state: &StorageState, sectors: Range<u64>| match owner {
            Some(owner) => state.weight_range(owner, path, sectors),
            None => f64::from(u8::from(state.eligible_range(path, sectors))),
        };
        let range_weight = weigh_range(&self.state, sectors.clone());
        let random_eio = config.write_eio_probability > 0.0
            && eio_roll < weighted(config.write_eio_probability, range_weight);
        if targeted || random_eio {
            assert_reachable!("disk fault: write failed with EIO");
            return WriteLanding::Eio;
        }

        let phantom_roll = sim_random::<f64>();
        let misdirect_roll = sim_random::<f64>();
        if config.phantom_write_probability > 0.0
            && phantom_roll < weighted(config.phantom_write_probability, range_weight)
        {
            assert_reachable!("disk fault: phantom write dropped");
            return WriteLanding::Phantom;
        }

        if config.misdirect_write_probability > 0.0
            && misdirect_roll < config.misdirect_write_probability
        {
            let max_offset = file_size.saturating_sub(len as u64);
            // Every offset a `len`-byte write fits at, `max_offset` included
            // (the misdirected-read path draws the same range).
            let mistaken = if max_offset > 0 {
                sim_random_range(0..max_offset + 1)
            } else {
                0
            };
            let mistaken_sectors = sector_range(mistaken, len);
            // A misdirection's draw is gated by the configured rate alone,
            // so the focus only grants immunity here, never extra hits.
            if mistaken != offset
                && range_weight > 0.0
                && weigh_range(&self.state, mistaken_sectors) > 0.0
            {
                assert_reachable!("disk fault: misdirected write landed elsewhere");
                return WriteLanding::At(mistaken);
            }
        }
        WriteLanding::At(offset)
    }

    /// Roll write-time latent corruption over the sectors a write touched.
    /// Returns whether any sector was damaged.
    fn plant_write_corruption(
        &mut self,
        file_id: FileId,
        path: &str,
        offset: u64,
        len: usize,
        config: &StorageConfiguration,
    ) -> bool {
        if config.write_corruption_probability <= 0.0 {
            return false;
        }
        let owner = self.state.files.get(&file_id).map(|file| file.owner_ip);
        let mut corrupted = false;
        for sector in sector_range(offset, len) {
            let roll = sim_random::<f64>();
            let weight = owner.map_or(0.0, |owner| self.state.weight(owner, path, sector));
            if roll < weighted(config.write_corruption_probability, weight) {
                self.plant_latent(file_id, sector);
                corrupted = true;
            }
        }
        if corrupted {
            assert_reachable!("disk fault: write planted latent corruption");
        }
        corrupted
    }

    pub(super) fn complete_sync(
        &mut self,
        pending: &PendingStorageOp,
        actions: &mut StorageActions,
    ) -> Result<(), StorageError> {
        let Some((owner_ip, path)) = self
            .state
            .files
            .get(&pending.file_id)
            .map(|file| (file.owner_ip, file.path.clone()))
        else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        let config = self.state.config_for(owner_ip).clone();
        if config.sync_failure_probability > 0.0
            && sim_random::<f64>() < config.sync_failure_probability
        {
            assert_reachable!("disk fault: sync failed");
            self.record(&path, StorageFaultKind::SyncFailure, None);
            actions.fault(SimFaultEvent::StorageSyncFault {
                ip: owner_ip.to_string(),
                file_id: pending.file_id.0,
            });
            return Err(StorageError::Io {
                file_id: pending.file_id,
                kind: std::io::ErrorKind::Other,
                message: "sync failed (simulated I/O error)".to_string(),
            });
        }

        // A lying sync reports a sector durable while leaving it volatile;
        // the crash oracle later reports what that lie cost.
        self.state.barrier_violation_armed |= config.barrier_violation_probability > 0.0;
        self.state.settle_turns(owner_ip);
        let weigh = self.state.weigher(owner_ip, &path);
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        file.image.sync(&config, &weigh);
        Ok(())
    }

    pub(super) fn complete_set_len(
        &mut self,
        pending: &PendingStorageOp,
        new_len: u64,
    ) -> Result<(), StorageError> {
        let Some(file) = self.state.files.get_mut(&pending.file_id) else {
            return Err(StorageError::InvalidFileHandle {
                handle_id: pending.handle_id,
            });
        };
        file.image.set_len(new_len);
        self.state.note_damage_changed();
        Ok(())
    }

    /// Land a write at `landed_at` (where the disk put it, `offset` being
    /// where it was aimed), keeping the damage marks a rolling replicated
    /// fault pattern reads.
    fn land_write(
        &mut self,
        file_id: FileId,
        handle_id: HandleId,
        landed_at: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let Some(file) = self.state.files.get_mut(&file_id) else {
            return Err(StorageError::InvalidFileHandle { handle_id });
        };
        if file.image.write(landed_at, data) {
            self.state.note_damage_changed();
        }
        if landed_at != offset {
            // Both the bytes it clobbered and the ones it missed.
            self.mark_damaged(file_id, sector_range(landed_at, data.len()));
            self.mark_damaged(file_id, sector_range(offset, data.len()));
        }
        Ok(())
    }
}
