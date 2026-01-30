//! Simulated storage provider implementation.

use super::file::SimStorageFile;
use crate::SimulationError;
use crate::sim::WeakSimWorld;
use async_trait::async_trait;
use moonpool_core::{OpenOptions, StorageProvider};
use std::io;

/// Map a SimulationError to an appropriate io::Error kind.
fn map_sim_error(e: SimulationError) -> io::Error {
    let msg = e.to_string();
    if msg.contains("already exists") {
        io::Error::new(io::ErrorKind::AlreadyExists, msg)
    } else if msg.contains("not found") {
        io::Error::new(io::ErrorKind::NotFound, msg)
    } else {
        io::Error::other(msg)
    }
}

/// Simulated storage provider for deterministic testing.
///
/// This provider wraps a `WeakSimWorld` and implements the `StorageProvider`
/// trait to enable deterministic storage simulation with fault injection.
///
/// # Example
///
/// ```ignore
/// let sim = SimWorld::new();
/// let provider = SimStorageProvider::new(sim.weak());
///
/// let file = provider.open("test.txt", OpenOptions::create_write()).await?;
/// ```
#[derive(Debug, Clone)]
pub struct SimStorageProvider {
    sim: WeakSimWorld,
}

impl SimStorageProvider {
    /// Create a new simulated storage provider.
    pub fn new(sim: WeakSimWorld) -> Self {
        Self { sim }
    }
}

#[async_trait(?Send)]
impl StorageProvider for SimStorageProvider {
    type File = SimStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // open_file schedules OpenComplete event with latency
        let file_id = sim.open_file(path, options, 0).map_err(map_sim_error)?;

        Ok(SimStorageFile::new(self.sim.clone(), file_id))
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        Ok(sim.file_exists(path))
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        sim.delete_file(path).map_err(map_sim_error)
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;
        sim.rename_file(from, to).map_err(map_sim_error)
    }
}
