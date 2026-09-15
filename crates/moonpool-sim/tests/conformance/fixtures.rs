//! Per-runtime environmental fixtures for the conformance contracts.
//!
//! The contract bodies are identical across providers; the only things that
//! genuinely differ between a real Tokio runtime and the simulation are
//! network addressing and storage paths. `Fixtures` captures exactly that seam
//! so the same contract can run against both.

#[cfg(feature = "tokio-providers")]
use tempfile::TempDir;

/// Environmental knobs a contract needs that differ per runtime.
pub(crate) trait Fixtures {
    /// Address to pass to `bind()`. Tokio: `"127.0.0.1:0"` (OS-assigned port).
    fn bind_addr(&self) -> String;

    /// Address to `connect()` to, derived from the bound listener's
    /// `local_addr()`. Tokio returns the resolved `127.0.0.1:<port>`.
    fn connect_addr(&self, listener_local_addr: &str) -> String;

    /// A filesystem path for a logical file name. Tokio places it under a
    /// private `TempDir` so tests stay isolated and self-cleaning.
    #[cfg(feature = "tokio-providers")]
    fn path(&self, name: &str) -> String;

    /// An address that nobody is listening on, where `connect()` must fail.
    /// Returns `None` for runtimes that don't reject unknown targets (so the
    /// contract skips that assertion there).
    fn unbound_addr(&self) -> Option<String>;
}

/// Fixtures for the real Tokio runtime: loopback sockets + a temp directory.
#[cfg(feature = "tokio-providers")]
pub(crate) struct TokioFixtures {
    dir: TempDir,
}

#[cfg(feature = "tokio-providers")]
impl TokioFixtures {
    pub(crate) fn new() -> Self {
        Self {
            dir: TempDir::new().expect("failed to create temp dir"),
        }
    }
}

#[cfg(feature = "tokio-providers")]
impl Fixtures for TokioFixtures {
    fn bind_addr(&self) -> String {
        "127.0.0.1:0".to_string()
    }

    fn connect_addr(&self, listener_local_addr: &str) -> String {
        listener_local_addr.to_string()
    }

    #[cfg(feature = "tokio-providers")]
    fn path(&self, name: &str) -> String {
        self.dir
            .path()
            .join(name)
            .to_str()
            .expect("temp path is valid UTF-8")
            .to_string()
    }

    fn unbound_addr(&self) -> Option<String> {
        // Port 1 on loopback is privileged and unbound: connect is refused fast.
        Some("127.0.0.1:1".to_string())
    }
}

/// Fixtures for the deterministic simulator: a process-local logical address.
pub(crate) struct SimFixtures;

impl SimFixtures {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl Fixtures for SimFixtures {
    fn bind_addr(&self) -> String {
        "10.0.1.1:0".to_string()
    }

    fn connect_addr(&self, listener_local_addr: &str) -> String {
        listener_local_addr.to_string()
    }

    #[cfg(feature = "tokio-providers")]
    fn path(&self, name: &str) -> String {
        name.to_string()
    }

    fn unbound_addr(&self) -> Option<String> {
        Some("10.0.1.1:1".to_string())
    }
}
