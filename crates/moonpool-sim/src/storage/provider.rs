//! Simulated storage provider implementation.

use super::file::SimStorageFile;
use crate::sim::WeakSimWorld;
use moonpool_core::{OpenOptions, StorageProvider};
use std::io;
use std::net::IpAddr;
use tracing::instrument;

/// Simulated storage provider for deterministic testing.
///
/// Each provider is scoped to a specific process IP address. Files opened
/// through this provider are tagged with the owner IP, enabling per-process
/// fault injection and storage isolation.
///
/// # Example
///
/// Inside a simulation, take it from
/// [`SimContext::storage`](crate::SimContext::storage). A test that drives
/// a [`SimWorld`](crate::SimWorld) by hand builds one per process:
///
/// ```rust
/// use moonpool_sim::{SimStorageProvider, SimWorld};
///
/// let sim = SimWorld::new();
/// let ip = std::net::IpAddr::from([10, 0, 1, 1]);
/// let _provider = SimStorageProvider::new(sim.downgrade(), ip);
/// // provider.open(...) returns a future the world's events complete.
/// ```
#[derive(Debug, Clone)]
pub struct SimStorageProvider {
    sim: WeakSimWorld,
    /// IP address of the process that owns files opened through this provider.
    owner_ip: IpAddr,
}

impl SimStorageProvider {
    /// Create a new simulated storage provider scoped to a process IP.
    #[must_use]
    pub fn new(sim: WeakSimWorld, owner_ip: IpAddr) -> Self {
        Self { sim, owner_ip }
    }

    /// Weigh where random faults land on this process's disk: the sectors a
    /// [`FaultFocus`](super::FaultFocus) names get its weights, every other
    /// one its background weight. Replaces any focus the disk had; a wipe of
    /// this process's storage clears it. A harness that knows its on-disk
    /// layout calls this to aim the configured fault families at the bytes
    /// that matter.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn focus_faults(&self, focus: super::FaultFocus) -> io::Result<()> {
        self.sim()?.set_fault_focus(self.owner_ip, focus);
        Ok(())
    }

    /// Remove this process's fault focus.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn clear_fault_focus(&self) -> io::Result<()> {
        self.sim()?.clear_fault_focus(self.owner_ip);
        Ok(())
    }

    /// Tell the simulator what this process's disk holds: its storage
    /// format's regions, each with the replicated record it holds (its
    /// `stripe`). A run with
    /// [`SimulationBuilder::replicated_storage_faults`](crate::SimulationBuilder::replicated_storage_faults)
    /// reads it to damage every record on some replicas and never on all;
    /// without it, publishing changes nothing. Replaces what was published
    /// before; a wipe of this process's storage clears it.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn publish_layout(&self, regions: &[moonpool_core::LayoutRegion]) -> io::Result<()> {
        self.sim()?.publish_layout(self.owner_ip, regions);
        Ok(())
    }

    /// Durably damage `bytes` of this process's file at `path`, unless the
    /// shard gate refuses it (see
    /// [`SimWorld::corrupt_process_file_bytes_gated`](crate::SimWorld::corrupt_process_file_bytes_gated)).
    /// The in-process twin of
    /// [`FaultContext::corrupt_bytes`](crate::FaultContext::corrupt_bytes):
    /// a harness that aims damage from inside a process places it where
    /// the simulator sees it. Returns whether the bytes changed. Takes no
    /// simulated time and draws no randomness.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down, there is no file at
    /// `path`, or `bytes` reach past its durable end.
    pub fn corrupt_bytes(&self, path: &str, bytes: std::ops::Range<u64>) -> io::Result<bool> {
        self.sim()?
            .corrupt_process_file_bytes_gated(self.owner_ip, path, bytes)
            .map_err(io::Error::from)
    }

    /// Durably damage `bytes` of this process's file at `path` without
    /// asking the shard gate: damage a scenario places on purpose past a
    /// shard's need (see
    /// [`SimWorld::corrupt_process_file_bytes`](crate::SimWorld::corrupt_process_file_bytes)).
    /// The gate still sees it, so it places nothing more on that stripe.
    /// Takes no simulated time and draws no randomness.
    ///
    /// # Errors
    ///
    /// As [`corrupt_bytes`](Self::corrupt_bytes).
    pub fn corrupt_bytes_past_gate(
        &self,
        path: &str,
        bytes: std::ops::Range<u64>,
    ) -> io::Result<()> {
        self.sim()?
            .corrupt_process_file_bytes(self.owner_ip, path, bytes)
            .map_err(io::Error::from)
    }

    /// Ask the shard gate whether this process may lose every copy it
    /// holds, and count the loss if so (see
    /// [`SimWorld::lose_replica_gated`](crate::SimWorld::lose_replica_gated)).
    /// A harness calls it before it destroys this process's data itself.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn lose_replica(&self) -> io::Result<bool> {
        Ok(self.sim()?.lose_replica_gated(self.owner_ip))
    }

    /// Count every copy this process holds lost, past the shard gate (see
    /// [`SimWorld::lose_replica_past_gate`](crate::SimWorld::lose_replica_past_gate)).
    /// A harness calls it when the process finds its data lost for good
    /// after a fault the gate let land.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn lose_replica_past_gate(&self) -> io::Result<()> {
        self.sim()?.lose_replica_past_gate(self.owner_ip);
        Ok(())
    }

    /// Count this process's copy of `shard` alone lost, past the shard gate
    /// (see [`SimWorld::lose_shard_replica_past_gate`](crate::SimWorld::lose_shard_replica_past_gate)).
    /// A harness calls it when the process gives up one store for good and
    /// keeps the others.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn lose_shard_past_gate(&self, shard: &str) -> io::Result<()> {
        self.sim()?
            .lose_shard_replica_past_gate(self.owner_ip, shard);
        Ok(())
    }

    /// The replicated fault pattern this seed drew for this process's
    /// group, if the run covers it.
    ///
    /// # Errors
    ///
    /// An I/O error if the simulation has shut down.
    pub fn fault_pattern(&self) -> io::Result<Option<super::FaultPattern>> {
        Ok(self.sim()?.fault_pattern(self.owner_ip))
    }

    /// Upgrade the weak simulation handle, mapping a dropped simulation to an
    /// I/O error so storage operations can propagate it with `?`.
    fn sim(&self) -> io::Result<crate::sim::SimWorld> {
        self.sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))
    }
}

impl StorageProvider for SimStorageProvider {
    type File = SimStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let sim = self.sim()?;

        let handle_id = sim.open_file(path, options, 0, self.owner_ip)?;
        let constraints = sim.handle_constraints(handle_id)?;
        let direct_io = sim.handle_is_direct_io(handle_id)?;

        Ok(SimStorageFile::new(
            self.sim.clone(),
            handle_id,
            constraints,
            direct_io,
        ))
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        Ok(self.sim()?.file_exists(self.owner_ip, path)?)
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        Ok(self.sim()?.delete_file(self.owner_ip, path)?)
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        Ok(self.sim()?.rename_file(self.owner_ip, from, to)?)
    }

    #[instrument(skip(self))]
    async fn create_dir_all(&self, path: &str) -> io::Result<()> {
        Ok(self.sim()?.create_dir_all(path, self.owner_ip)?)
    }

    async fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        Ok(self.sim()?.list_dir(path, self.owner_ip)?)
    }

    async fn sync_dir(&self, path: &str) -> io::Result<()> {
        Ok(self.sim()?.sync_dir(path, self.owner_ip)?)
    }

    fn publish_store_layout(
        &self,
        store: &str,
        regions: impl FnOnce() -> Vec<moonpool_core::LayoutRegion>,
    ) {
        // Only a shut-down simulation refuses, and then nothing is placed.
        if let Ok(sim) = self.sim() {
            sim.publish_store_layout(self.owner_ip, store, &regions());
        }
    }
}
