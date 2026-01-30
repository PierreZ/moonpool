//! Storage I/O operations for the simulation.
//!
//! This module contains storage-related event handlers and methods extracted from
//! `world.rs` to improve code organization. It handles file operations like
//! open, read, write, sync, and provides fault injection for testing storage reliability.

use std::task::Waker;
use std::time::Duration;

use crate::{SimulationError, SimulationResult};

use super::{
    events::{Event, ScheduledEvent, StorageOperation},
    rng::{sim_random, sim_random_range},
    state::{FileId, PendingOpType, PendingStorageOp},
    world::{SimInner, SimWorld},
};

// =============================================================================
// Storage Event Handlers
// =============================================================================

/// Handle storage I/O events.
///
/// Storage events represent the completion of I/O operations.
/// Processing applies faults and wakes waiting tasks.
pub(crate) fn handle_storage_event(
    inner: &mut SimInner,
    file_id: u64,
    operation: StorageOperation,
) {
    let file_id = FileId(file_id);

    match operation {
        StorageOperation::ReadComplete { len: _ } => {
            handle_read_complete(inner, file_id);
        }
        StorageOperation::WriteComplete { len: _ } => {
            handle_write_complete(inner, file_id);
        }
        StorageOperation::SyncComplete => {
            handle_sync_complete(inner, file_id);
        }
        StorageOperation::OpenComplete => {
            handle_open_complete(inner, file_id);
        }
        StorageOperation::SetLenComplete { new_len } => {
            handle_set_len_complete(inner, file_id, new_len);
        }
    }
}

/// Handle read operation completion.
fn handle_read_complete(inner: &mut SimInner, file_id: FileId) {
    let read_fault_probability = inner.storage.config.read_fault_probability;

    // Find and remove the oldest pending read operation
    let op_info = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
        // Find the first pending read operation
        let op_info = file_state
            .pending_ops
            .iter()
            .find(|(_, op)| op.op_type == PendingOpType::Read)
            .map(|(&seq, op)| (seq, op.offset, op.len));

        if let Some((seq, _, _)) = op_info {
            file_state.pending_ops.remove(&seq);
        }
        op_info
    } else {
        tracing::warn!("ReadComplete for unknown file {:?}", file_id);
        return;
    };

    // Apply read fault injection - mark sectors as faulted based on probability
    if let Some((op_seq, offset, len)) = op_info {
        if read_fault_probability > 0.0
            && let Some(file_state) = inner.storage.files.get_mut(&file_id)
        {
            let start_sector = (offset as usize) / crate::storage::SECTOR_SIZE;
            let end_sector = (offset as usize + len).div_ceil(crate::storage::SECTOR_SIZE);

            for sector in start_sector..end_sector {
                if sim_random::<f64>() < read_fault_probability {
                    file_state.storage.set_fault(sector);
                    tracing::info!(
                        "Read fault injected for file {:?}, sector {}",
                        file_id,
                        sector
                    );
                }
            }
        }

        // Wake the waker for this operation
        if let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, op_seq)) {
            tracing::trace!("Waking read waker for file {:?}, op {}", file_id, op_seq);
            waker.wake();
        }
    }
}

/// Handle write operation completion.
///
/// Applies the write to storage with potential fault injection:
/// - phantom_write_probability: write appears to succeed but isn't persisted
/// - misdirect_write_probability: write lands at wrong location
fn handle_write_complete(inner: &mut SimInner, file_id: FileId) {
    let config = inner.storage.config.clone();

    // Find the oldest pending write operation and extract its data
    let op_info = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
        let op_seq = file_state
            .pending_ops
            .iter()
            .find(|(_, op)| op.op_type == PendingOpType::Write)
            .map(|(&seq, _)| seq);

        if let Some(seq) = op_seq {
            let op = file_state.pending_ops.remove(&seq);
            op.map(|o| (seq, o.offset, o.data))
        } else {
            None
        }
    } else {
        tracing::warn!("WriteComplete for unknown file {:?}", file_id);
        return;
    };

    // Apply the write with potential fault injection
    if let Some((op_seq, offset, data_opt)) = op_info {
        if let Some(data) = data_opt
            && let Some(file_state) = inner.storage.files.get_mut(&file_id)
        {
            // Check for phantom write (write appears to succeed but doesn't persist)
            if sim_random::<f64>() < config.phantom_write_probability {
                tracing::info!(
                    "Phantom write injected for file {:?}, offset {}, len {}",
                    file_id,
                    offset,
                    data.len()
                );
                file_state.storage.record_phantom_write(offset, &data);
            }
            // Check for misdirected write
            else if sim_random::<f64>() < config.misdirect_write_probability {
                // Pick a random different offset
                let max_offset = file_state.storage.size().saturating_sub(data.len() as u64);
                let mistaken_offset = if max_offset > 0 {
                    sim_random_range(0..max_offset)
                } else {
                    0
                };
                tracing::info!(
                    "Misdirected write injected for file {:?}: intended={}, actual={}",
                    file_id,
                    offset,
                    mistaken_offset
                );
                if let Err(e) =
                    file_state
                        .storage
                        .apply_misdirected_write(offset, mistaken_offset, &data)
                {
                    tracing::warn!("Failed to apply misdirected write: {}", e);
                }
            }
            // Normal write (not synced - may be lost on crash)
            else if let Err(e) = file_state.storage.write(offset, &data, false) {
                tracing::warn!("Write failed for file {:?}: {}", file_id, e);
            } else {
                // Check for write corruption - mark sectors as faulted after successful write
                if config.write_fault_probability > 0.0 {
                    let start_sector = (offset as usize) / crate::storage::SECTOR_SIZE;
                    let end_sector =
                        (offset as usize + data.len()).div_ceil(crate::storage::SECTOR_SIZE);

                    for sector in start_sector..end_sector {
                        if sim_random::<f64>() < config.write_fault_probability {
                            file_state.storage.set_fault(sector);
                            tracing::info!(
                                "Write fault injected for file {:?}, sector {}",
                                file_id,
                                sector
                            );
                        }
                    }
                }
            }
        }

        // Wake the waker for this operation
        if let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, op_seq)) {
            tracing::trace!("Waking write waker for file {:?}, op {}", file_id, op_seq);
            waker.wake();
        }
    }
}

/// Handle sync operation completion.
///
/// Applies sync_failure_probability fault injection.
fn handle_sync_complete(inner: &mut SimInner, file_id: FileId) {
    let sync_failure_prob = inner.storage.config.sync_failure_probability;

    // Find and remove the oldest pending sync operation
    let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
        let op_seq = file_state
            .pending_ops
            .iter()
            .find(|(_, op)| op.op_type == PendingOpType::Sync)
            .map(|(&seq, _)| seq);

        if let Some(seq) = op_seq {
            file_state.pending_ops.remove(&seq);

            // Check for sync failure
            if sim_random::<f64>() < sync_failure_prob {
                tracing::info!("Sync failure injected for file {:?}", file_id);
                // Record the failure so SyncFuture can return an error
                inner.storage.sync_failures.insert((file_id, seq));
                // On sync failure, we don't call storage.sync()
                // Data remains in pending state and may be lost on crash
            } else {
                // Successful sync - make all pending writes durable
                file_state.storage.sync();
            }
        }
        op_seq
    } else {
        tracing::warn!("SyncComplete for unknown file {:?}", file_id);
        return;
    };

    // Wake the waker for this operation
    if let Some(seq) = op_seq
        && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
    {
        tracing::trace!("Waking sync waker for file {:?}, op {}", file_id, seq);
        waker.wake();
    }
}

/// Handle open operation completion.
fn handle_open_complete(inner: &mut SimInner, file_id: FileId) {
    // Find and remove the oldest pending open operation
    let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
        let op_seq = file_state
            .pending_ops
            .iter()
            .find(|(_, op)| op.op_type == PendingOpType::Open)
            .map(|(&seq, _)| seq);

        if let Some(seq) = op_seq {
            file_state.pending_ops.remove(&seq);
        }
        op_seq
    } else {
        // File might not have pending open op (it was already "open" on creation)
        tracing::trace!("OpenComplete for file {:?} (no pending op)", file_id);
        return;
    };

    // Wake the waker for this operation
    if let Some(seq) = op_seq
        && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
    {
        tracing::trace!("Waking open waker for file {:?}, op {}", file_id, seq);
        waker.wake();
    }
}

/// Handle set_len operation completion.
fn handle_set_len_complete(inner: &mut SimInner, file_id: FileId, new_len: u64) {
    // Find and remove the oldest pending set_len operation
    let op_seq = if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
        let op_seq = file_state
            .pending_ops
            .iter()
            .find(|(_, op)| op.op_type == PendingOpType::SetLen)
            .map(|(&seq, _)| seq);

        if let Some(seq) = op_seq {
            file_state.pending_ops.remove(&seq);

            // Resize the storage
            // Note: InMemoryStorage doesn't have a resize method, so we create a new one
            // This is a simplification - in a real implementation we'd preserve existing data
            let seed = sim_random::<u64>();
            let mut new_storage = crate::storage::InMemoryStorage::new(new_len, seed);

            // Copy existing data up to the minimum of old and new sizes
            let copy_len = file_state.storage.size().min(new_len) as usize;
            if copy_len > 0 {
                let mut buf = vec![0u8; copy_len];
                if file_state.storage.read(0, &mut buf).is_ok() {
                    let _ = new_storage.write(0, &buf, true);
                }
            }
            file_state.storage = new_storage;
        }
        op_seq
    } else {
        tracing::warn!("SetLenComplete for unknown file {:?}", file_id);
        return;
    };

    // Wake the waker for this operation
    if let Some(seq) = op_seq
        && let Some(waker) = inner.wakers.storage_wakers.remove(&(file_id, seq))
    {
        tracing::trace!(
            "Waking set_len waker for file {:?}, op {}, new_len={}",
            file_id,
            seq,
            new_len
        );
        waker.wake();
    }
}

// =============================================================================
// Storage Methods for SimWorld
// =============================================================================

impl SimWorld {
    /// Access storage configuration for the simulation.
    pub fn with_storage_config<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&crate::storage::StorageConfiguration) -> R,
    {
        let inner = self.inner.borrow();
        f(&inner.storage.config)
    }

    /// Open a file in the simulation.
    ///
    /// Creates a new file or opens an existing one based on the options.
    /// Schedules an `OpenComplete` event and returns the file ID.
    pub(crate) fn open_file(
        &self,
        path: &str,
        options: moonpool_core::OpenOptions,
        initial_size: u64,
    ) -> SimulationResult<FileId> {
        use crate::storage::InMemoryStorage;

        let mut inner = self.inner.borrow_mut();
        let path_str = path.to_string();

        // Check create_new semantics - fail if file exists
        if options.create_new && inner.storage.path_to_file.contains_key(&path_str) {
            return Err(SimulationError::IoError(
                "File already exists (create_new)".to_string(),
            ));
        }

        // Check if file was deleted and create is not set
        if inner.storage.deleted_paths.contains(&path_str) && !options.create {
            return Err(SimulationError::IoError("File not found".to_string()));
        }

        // If file already exists and we're opening it, return existing file ID
        if let Some(&existing_id) = inner.storage.path_to_file.get(&path_str) {
            if let Some(file_state) = inner.storage.files.get_mut(&existing_id) {
                // If truncate is set, reset the storage
                if options.truncate {
                    let seed = sim_random::<u64>();
                    file_state.storage = InMemoryStorage::new(0, seed);
                    file_state.position = 0;
                } else if options.append {
                    // For append mode, seek to end
                    file_state.position = file_state.storage.size();
                } else {
                    // For normal reopen, reset position to start
                    file_state.position = 0;
                }
                // Update options for the new open
                file_state.options = options;
                file_state.is_closed = false;
            }
            return Ok(existing_id);
        }

        // File doesn't exist - check if we're allowed to create it
        if !options.create && !options.create_new {
            return Err(SimulationError::IoError("File not found".to_string()));
        }

        // Create new file
        let file_id = FileId(inner.storage.next_file_id);
        inner.storage.next_file_id += 1;

        // Remove from deleted paths if re-creating
        inner.storage.deleted_paths.remove(&path_str);

        // Create in-memory storage with deterministic seed
        let seed = sim_random::<u64>();
        let storage = InMemoryStorage::new(initial_size, seed);

        let file_state =
            super::state::StorageFileState::new(file_id, path_str.clone(), options, storage);

        inner.storage.files.insert(file_id, file_state);
        inner.storage.path_to_file.insert(path_str, file_id);

        // Schedule OpenComplete event with minimal latency
        let open_latency = Duration::from_micros(1);
        let scheduled_time = inner.current_time + open_latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::OpenComplete,
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::debug!("Opened file {:?} with id {:?}", path, file_id);
        Ok(file_id)
    }

    /// Check if a file exists at the given path.
    pub(crate) fn file_exists(&self, path: &str) -> bool {
        let inner = self.inner.borrow();
        let path_str = path.to_string();
        inner.storage.path_to_file.contains_key(&path_str)
            && !inner.storage.deleted_paths.contains(&path_str)
    }

    /// Delete a file at the given path.
    pub(crate) fn delete_file(&self, path: &str) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let path_str = path.to_string();

        if let Some(file_id) = inner.storage.path_to_file.remove(&path_str) {
            // Mark file as closed and remove it
            if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
                file_state.is_closed = true;
            }
            inner.storage.files.remove(&file_id);
            inner.storage.deleted_paths.insert(path_str);
            tracing::debug!("Deleted file {:?}", path);
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Rename a file from one path to another.
    pub(crate) fn rename_file(&self, from: &str, to: &str) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let from_str = from.to_string();
        let to_str = to.to_string();

        if let Some(file_id) = inner.storage.path_to_file.remove(&from_str) {
            // Update the path in the file state
            if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
                file_state.path = to_str.clone();
            }
            inner.storage.path_to_file.insert(to_str, file_id);
            inner.storage.deleted_paths.remove(&from_str);
            tracing::debug!("Renamed file {:?} to {:?}", from, to);
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Schedule a read operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_read(
        &self,
        file_id: FileId,
        offset: u64,
        len: usize,
    ) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Read,
                offset,
                len,
                data: None,
            },
        );

        // Calculate latency and schedule completion event
        let latency = Self::calculate_storage_latency(&inner.storage.config, len, false);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::ReadComplete { len: len as u32 },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled read: file={:?}, offset={}, len={}, op_seq={}",
            file_id,
            offset,
            len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Schedule a write operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_write(
        &self,
        file_id: FileId,
        offset: u64,
        data: Vec<u8>,
    ) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;
        let len = data.len();

        // Store the pending operation with the data
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Write,
                offset,
                len,
                data: Some(data),
            },
        );

        // Calculate latency and schedule completion event
        let latency = Self::calculate_storage_latency(&inner.storage.config, len, true);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::WriteComplete { len: len as u32 },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled write: file={:?}, offset={}, len={}, op_seq={}",
            file_id,
            offset,
            len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Schedule a sync operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_sync(&self, file_id: FileId) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::Sync,
                offset: 0,
                len: 0,
                data: None,
            },
        );

        // Use sync latency from config
        let latency = crate::network::sample_duration(&inner.storage.config.sync_latency);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::SyncComplete,
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!("Scheduled sync: file={:?}, op_seq={}", file_id, op_seq);

        Ok(op_seq)
    }

    /// Schedule a set_len operation on a file.
    ///
    /// Returns an operation sequence number that can be used to check completion.
    pub(crate) fn schedule_set_len(&self, file_id: FileId, new_len: u64) -> SimulationResult<u64> {
        let mut inner = self.inner.borrow_mut();

        let file_state = inner
            .storage
            .files
            .get_mut(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        let op_seq = file_state.next_op_seq;
        file_state.next_op_seq += 1;

        // Store the pending operation
        file_state.pending_ops.insert(
            op_seq,
            PendingStorageOp {
                op_type: PendingOpType::SetLen,
                offset: new_len,
                len: 0,
                data: None,
            },
        );

        // Use write latency for set_len
        let latency = crate::network::sample_duration(&inner.storage.config.write_latency);
        let scheduled_time = inner.current_time + latency;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;

        let event = Event::Storage {
            file_id: file_id.0,
            operation: StorageOperation::SetLenComplete { new_len },
        };
        inner
            .event_queue
            .schedule(ScheduledEvent::new(scheduled_time, event, sequence));

        tracing::trace!(
            "Scheduled set_len: file={:?}, new_len={}, op_seq={}",
            file_id,
            new_len,
            op_seq
        );

        Ok(op_seq)
    }

    /// Check if a storage operation is complete.
    pub(crate) fn is_storage_op_complete(&self, file_id: FileId, op_seq: u64) -> bool {
        let inner = self.inner.borrow();
        if let Some(file_state) = inner.storage.files.get(&file_id) {
            // Operation is complete when it's no longer in pending_ops
            !file_state.pending_ops.contains_key(&op_seq)
        } else {
            // File not found means operation is effectively "complete" (failed)
            true
        }
    }

    /// Check if a sync operation failed and clear the failure flag.
    ///
    /// Returns true if the sync failed due to fault injection.
    pub(crate) fn take_sync_failure(&self, file_id: FileId, op_seq: u64) -> bool {
        let mut inner = self.inner.borrow_mut();
        inner.storage.sync_failures.remove(&(file_id, op_seq))
    }

    /// Register a waker for a storage operation.
    pub(crate) fn register_storage_waker(&self, file_id: FileId, op_seq: u64, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        inner.wakers.storage_wakers.insert((file_id, op_seq), waker);
    }

    /// Read data from a file at the given offset.
    ///
    /// This is called after ReadComplete to actually fetch the data.
    pub(crate) fn read_from_file(
        &self,
        file_id: FileId,
        offset: u64,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let inner = self.inner.borrow();

        let file_state = inner
            .storage
            .files
            .get(&file_id)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))?;

        if file_state.is_closed {
            return Err(SimulationError::IoError("File is closed".to_string()));
        }

        // Read from the in-memory storage
        file_state
            .storage
            .read(offset, buf)
            .map_err(|e| SimulationError::IoError(e.to_string()))?;

        Ok(buf.len())
    }

    /// Get the current file position.
    pub(crate) fn get_file_position(&self, file_id: FileId) -> SimulationResult<u64> {
        let inner = self.inner.borrow();
        inner
            .storage
            .files
            .get(&file_id)
            .map(|f| f.position)
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))
    }

    /// Set the current file position.
    pub(crate) fn set_file_position(&self, file_id: FileId, position: u64) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        if let Some(file_state) = inner.storage.files.get_mut(&file_id) {
            file_state.position = position;
            Ok(())
        } else {
            Err(SimulationError::IoError("File not found".to_string()))
        }
    }

    /// Get the size of a file.
    pub(crate) fn get_file_size(&self, file_id: FileId) -> SimulationResult<u64> {
        let inner = self.inner.borrow();
        inner
            .storage
            .files
            .get(&file_id)
            .map(|f| f.storage.size())
            .ok_or_else(|| SimulationError::IoError("File not found".to_string()))
    }

    /// Calculate storage latency using FDB formula.
    ///
    /// Latency = base_latency + iops_overhead + transfer_time
    fn calculate_storage_latency(
        config: &crate::storage::StorageConfiguration,
        size: usize,
        is_write: bool,
    ) -> Duration {
        // Sample base latency from config range
        let base_range = if is_write {
            &config.write_latency
        } else {
            &config.read_latency
        };
        let base = crate::network::sample_duration(base_range);

        // IOPS overhead: 1/iops seconds per operation
        let iops_overhead = Duration::from_secs_f64(1.0 / config.iops as f64);

        // Transfer time: size / bandwidth seconds
        let transfer = Duration::from_secs_f64(size as f64 / config.bandwidth as f64);

        base + iops_overhead + transfer
    }

    /// Simulate a crash affecting storage.
    ///
    /// This applies crash simulation to all open files:
    /// 1. Calls `apply_crash()` on all `InMemoryStorage` instances
    /// 2. Clears pending operations (lost in crash)
    /// 3. Optionally marks files as closed
    /// 4. Wakes all storage wakers (operations will fail)
    pub fn simulate_crash(&self, close_files: bool) {
        let mut inner = self.inner.borrow_mut();
        let crash_probability = inner.storage.config.crash_fault_probability;

        // Collect all wakers to wake in one pass (to avoid borrow conflict)
        let mut wakers_to_wake = Vec::new();
        let file_ids: Vec<FileId> = inner.storage.files.keys().copied().collect();

        for file_id in &file_ids {
            if let Some(file_state) = inner.storage.files.get_mut(file_id) {
                // Apply crash to in-memory storage (may corrupt pending writes)
                file_state.storage.apply_crash(crash_probability);

                // Collect lost op sequence numbers
                let lost_ops: Vec<u64> = file_state.pending_ops.keys().copied().collect();

                // Clear pending ops - they're lost in crash
                file_state.pending_ops.clear();

                // Collect waker keys for later removal
                for op_seq in lost_ops {
                    wakers_to_wake.push((*file_id, op_seq));
                }

                // Optionally close files
                if close_files {
                    file_state.is_closed = true;
                }
            }
        }

        // Wake all collected wakers (after file iteration is complete)
        for key in wakers_to_wake {
            if let Some(waker) = inner.wakers.storage_wakers.remove(&key) {
                waker.wake();
            }
        }

        tracing::info!(
            "Storage crash simulated: {} files affected, close_files={}",
            file_ids.len(),
            close_files
        );
    }
}
