//! The local Tokio runtime the storage and chaos tests step a world from.
//!
//! Shared by the test binaries that include it with `#[path]`.

/// Create a current-thread Tokio runtime with I/O and time enabled.
pub fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}
