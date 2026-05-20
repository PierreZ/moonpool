//! Simulated storage provider implementation.

use super::file::SimStorageFile;
use crate::sim::WeakSimWorld;
use moonpool_core::{OpenOptions, StorageProvider};
use std::io;
use std::net::IpAddr;

/// Simulated storage provider for deterministic testing.
///
/// Each provider is scoped to a specific process IP address. Files opened
/// through this provider are tagged with the owner IP, enabling per-process
/// fault injection and storage isolation.
///
/// # Example
///
/// ```ignore
/// let sim = SimWorld::new();
/// let ip: IpAddr = "10.0.1.1".parse().unwrap();
/// let provider = SimStorageProvider::new(sim.weak(), ip);
///
/// let file = provider.open("test.txt", OpenOptions::create_write()).await?;
/// ```
#[derive(Debug, Clone)]
pub struct SimStorageProvider {
    sim: WeakSimWorld,
    /// IP address of the process that owns files opened through this provider.
    owner_ip: IpAddr,
}

impl SimStorageProvider {
    /// Create a new simulated storage provider scoped to a process IP.
    pub fn new(sim: WeakSimWorld, owner_ip: IpAddr) -> Self {
        Self { sim, owner_ip }
    }
}

impl StorageProvider for SimStorageProvider {
    type File = SimStorageFile;

    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl std::future::Future<Output = io::Result<Self::File>> + Send {
        let sim_weak = self.sim.clone();
        let owner_ip = self.owner_ip;
        let path = path.to_string();
        async move {
            let sim = sim_weak
                .upgrade()
                .map_err(|_| io::Error::other("simulation shutdown"))?;

            let file_id = sim.open_file(&path, options, 0, owner_ip)?;

            Ok(SimStorageFile::new(sim_weak, file_id))
        }
    }

    fn exists(&self, path: &str) -> impl std::future::Future<Output = io::Result<bool>> + Send {
        let sim_weak = self.sim.clone();
        let path = path.to_string();
        async move {
            let sim = sim_weak
                .upgrade()
                .map_err(|_| io::Error::other("simulation shutdown"))?;
            Ok(sim.file_exists(&path))
        }
    }

    fn delete(&self, path: &str) -> impl std::future::Future<Output = io::Result<()>> + Send {
        let sim_weak = self.sim.clone();
        let path = path.to_string();
        async move {
            let sim = sim_weak
                .upgrade()
                .map_err(|_| io::Error::other("simulation shutdown"))?;
            sim.delete_file(&path)?;
            Ok(())
        }
    }

    fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send {
        let sim_weak = self.sim.clone();
        let from = from.to_string();
        let to = to.to_string();
        async move {
            let sim = sim_weak
                .upgrade()
                .map_err(|_| io::Error::other("simulation shutdown"))?;
            sim.rename_file(&from, &to)?;
            Ok(())
        }
    }
}
