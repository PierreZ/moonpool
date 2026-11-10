//! Namespace operations as delayed disk operations.
//!
//! `open`, `delete`, `rename`, `create_dir_all` and `sync_dir` wait in the
//! disk queue like any other operation and take effect when their completion
//! runs, as the `FoundationDB` simulator delays `open`, `deleteFile` and
//! `renameFile`. So a crash can land between a `rename` and its `sync_dir`,
//! a failed disk hangs them, and a stall delays them. `exists` and `list_dir`
//! read the cached namespace and stay immediate.

use std::{net::IpAddr, time::Duration};

use super::{
    OperationId, StorageEvent,
    engine::{StorageActions, StorageCompletion, StorageEngine, service_time},
    state::{NamespaceOp, PendingNamespaceOp, Wait},
};
use crate::{
    assert_reachable,
    network::sample_latency,
    sim::wakers::WakeBatch,
    storage::{StorageError, StorageOperation},
};

impl NamespaceOp {
    /// Whether the operation changes the disk. A dropped mutation still
    /// lands; a dropped read-only open is cancelled.
    fn is_mutation(&self) -> bool {
        match self {
            Self::Open { options, .. } => {
                options.is_create() || options.is_create_new() || options.is_truncate()
            }
            Self::Delete { .. }
            | Self::Rename { .. }
            | Self::CreateDirAll { .. }
            | Self::SyncDir { .. } => true,
        }
    }
}

impl StorageEngine {
    /// Submit one namespace operation on `owner_ip`'s disk at `now`.
    ///
    /// It waits in the disk queue for one I/O and then the configured
    /// [`namespace_latency`](crate::StorageConfiguration::namespace_latency).
    /// On a failed disk it parks for good, as every other operation does. It
    /// never rolls the disk-failure coin itself, so namespace traffic does
    /// not make disks fail more often.
    pub(crate) fn schedule_namespace(
        &mut self,
        owner_ip: IpAddr,
        op: NamespaceOp,
        now: Duration,
    ) -> (OperationId, StorageActions) {
        self.set_clock(now);
        let mut actions = StorageActions::default();
        let operation_id = OperationId(self.state.next_operation_id);
        self.state.next_operation_id += 1;
        let parked = self.state.failed_disks.contains(&owner_ip);
        self.state.namespace_ops.insert(
            operation_id,
            PendingNamespaceOp {
                owner_ip,
                op,
                wait: if parked { Wait::Parked } else { Wait::Awaited },
            },
        );
        if parked {
            assert_reachable!("disk: a namespace operation parks on a failed disk");
            return (operation_id, actions);
        }
        let episode = self.update_disk_episode(owner_ip, now);
        let config = self.state.config_for(owner_ip);
        let base = sample_latency(&config.namespace_latency);
        let service = service_time(config, 0, episode);
        let latency = self.queued_latency(owner_ip, episode, service, base, now);
        actions.schedule(
            now.saturating_add(latency),
            StorageEvent::new(operation_id, None, StorageOperation::NamespaceComplete),
        );
        (operation_id, actions)
    }

    /// Remove one namespace operation from the in-flight set. It never
    /// counted in the disk's activity: `/proc/diskstats` counts block reads
    /// and writes, not the metadata operations that cause them.
    fn take_namespace(&mut self, operation_id: OperationId) -> Option<PendingNamespaceOp> {
        self.state.namespace_ops.remove(&operation_id)
    }

    /// Whether `operation_id` is a namespace operation still in flight.
    pub(super) fn namespace_in_flight(&self, operation_id: OperationId) -> bool {
        self.state.namespace_ops.contains_key(&operation_id)
    }

    /// Apply one namespace operation whose completion runs now.
    pub(super) fn complete_namespace(&mut self, operation_id: OperationId) -> StorageActions {
        let mut actions = StorageActions::default();
        let Some(pending) = self.take_namespace(operation_id) else {
            return actions;
        };
        let ip = pending.owner_ip;
        let result = match pending.op {
            NamespaceOp::Open { path, options } => self
                .open_file(&path, options, 0, ip)
                .map(StorageCompletion::Opened),
            NamespaceOp::Delete { path } => self.delete_file(ip, &path).map(|effects| {
                actions.absorb(effects);
                StorageCompletion::Unit
            }),
            NamespaceOp::Rename { from, to } => self.rename_file(ip, &from, &to).map(|effects| {
                actions.absorb(effects);
                StorageCompletion::Unit
            }),
            NamespaceOp::CreateDirAll { path } => self.create_dir_all(&path, ip).map(|effects| {
                actions.absorb(effects);
                StorageCompletion::Unit
            }),
            NamespaceOp::SyncDir { path } => {
                let (result, effects) = self.sync_dir(&path, ip);
                actions.absorb(effects);
                result.map(|()| StorageCompletion::Unit)
            }
        };
        if pending.wait == Wait::Detached {
            // Its future is gone: the effect is on the disk, and a handle
            // nobody will ever use is closed at once.
            assert_reachable!("disk: a dropped namespace operation landed with no waiter");
            if let Ok(StorageCompletion::Opened(handle_id)) = result {
                actions.absorb(self.close_handle(handle_id));
            }
        } else {
            self.results.insert(operation_id, result);
        }
        actions.wakes.push(self.wakers.take(&operation_id));
        actions
    }

    /// The future waiting on namespace operation `operation_id` was
    /// dropped. A mutation is detached and still lands, with no discard
    /// coin; a read-only open, or anything parked on a failed disk, is
    /// cancelled. Returns whether `operation_id` was a namespace operation.
    pub(super) fn drop_namespace(
        &mut self,
        operation_id: OperationId,
        actions: &mut StorageActions,
    ) -> bool {
        let Some(pending) = self.state.namespace_ops.get_mut(&operation_id) else {
            return false;
        };
        match pending.wait {
            Wait::Detached => {}
            Wait::Awaited if pending.op.is_mutation() => {
                assert_reachable!("disk: a dropped namespace operation still lands");
                pending.wait = Wait::Detached;
            }
            Wait::Awaited | Wait::Parked => {
                self.take_namespace(operation_id);
                actions.cancel(operation_id);
            }
        }
        true
    }

    /// Fail a namespace operation the scheduler refused. Returns `None` if
    /// `operation_id` is not one.
    pub(super) fn fail_scheduled_namespace(
        &mut self,
        operation_id: OperationId,
    ) -> Option<WakeBatch> {
        let pending = self.take_namespace(operation_id)?;
        if pending.wait != Wait::Detached {
            self.results.insert(
                operation_id,
                Err(StorageError::ScheduleFailed { operation_id }),
            );
        }
        let mut wakes = WakeBatch::default();
        wakes.push(self.wakers.take(&operation_id));
        Some(wakes)
    }

    /// Fail every namespace operation in flight on `ip`'s disk (a crash or
    /// a wipe): none of them takes effect.
    pub(super) fn fail_namespace_ops(&mut self, ip: IpAddr, actions: &mut StorageActions) {
        let operation_ids: Vec<OperationId> = self
            .state
            .namespace_ops
            .iter()
            .filter(|(_, pending)| pending.owner_ip == ip)
            .map(|(operation_id, _)| *operation_id)
            .collect();
        for operation_id in operation_ids {
            self.fail_namespace(
                operation_id,
                StorageError::NamespaceInterrupted { operation_id },
                actions,
            );
        }
    }

    /// Fail every namespace operation in flight (simulation shutdown).
    pub(super) fn shut_down_namespace_ops(&mut self, actions: &mut StorageActions) {
        let operation_ids: Vec<OperationId> = self.state.namespace_ops.keys().copied().collect();
        for operation_id in operation_ids {
            self.fail_namespace(
                operation_id,
                StorageError::SimulationShutdown { operation_id },
                actions,
            );
        }
    }

    fn fail_namespace(
        &mut self,
        operation_id: OperationId,
        error: StorageError,
        actions: &mut StorageActions,
    ) {
        let Some(pending) = self.take_namespace(operation_id) else {
            return;
        };
        if pending.wait != Wait::Detached {
            self.results.insert(operation_id, Err(error));
        }
        actions.cancel(operation_id);
        actions.wakes.push(self.wakers.take(&operation_id));
    }
}
