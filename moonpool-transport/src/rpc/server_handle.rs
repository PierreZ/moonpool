//! Server handle for managing running RPC server tasks.

/// Handle for a running RPC server.
///
/// Created by the generated `serve()` method on `{Interface}Server` types.
/// Closing the handle (via [`stop()`](Self::stop) or [`Drop`]) shuts down
/// all serving tasks by closing the underlying request streams.
pub struct ServerHandle {
    close_fns: Vec<Box<dyn Fn()>>,
}

impl ServerHandle {
    /// Create a new server handle from a set of close functions.
    pub fn new(close_fns: Vec<Box<dyn Fn()>>) -> Self {
        Self { close_fns }
    }

    /// Stop all serving tasks by closing the underlying request streams.
    ///
    /// Calling `stop()` multiple times is safe — close operations are idempotent.
    pub fn stop(&self) {
        for f in &self.close_fns {
            f();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
