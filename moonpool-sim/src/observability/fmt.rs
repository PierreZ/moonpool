//! `tracing_subscriber::fmt` integration that uses simulation time as the
//! timestamp prefix.
//!
//! By default, `tracing_subscriber::fmt::layer()` prints wall-clock time —
//! useless in simulation, where logical time skips idle periods and seconds
//! of simulated activity may take milliseconds of wall time. [`SimTime`]
//! reads the current simulation time from a [`SimulationLayerHandle`] and
//! prints it instead.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::{SimTime, SimulationLayer};
//! use tracing_subscriber::layer::SubscriberExt;
//!
//! let layer = SimulationLayer::new();
//! let handle = layer.handle();
//!
//! // Important: register `SimulationLayer` BEFORE the fmt layer so it runs
//! // first and updates `current_sim_time_ms` before fmt formats the event.
//! let subscriber = tracing_subscriber::registry()
//!     .with(layer)
//!     .with(
//!         tracing_subscriber::fmt::layer()
//!             .with_timer(SimTime::new(handle.clone())),
//!     );
//!
//! let _guard = tracing::subscriber::set_default(subscriber);
//! ```
//!
//! Output looks like:
//!
//! ```text
//! sim+    1.234s  INFO moonpool::sim: key="delivery.at_most_once" payload=Replied { seq_id: 7 }
//! ```

use std::fmt;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

use super::layer::SimulationLayerHandle;

/// `FormatTime` implementation that writes the current simulation time
/// instead of wall-clock time.
///
/// Holds a [`SimulationLayerHandle`] and reads
/// [`SimulationLayerHandle::current_sim_time_ms`] each time the formatter
/// is invoked — so two parallel simulations in the same process keep their
/// clocks separate via separate handles.
#[derive(Clone)]
pub struct SimTime {
    handle: SimulationLayerHandle,
}

impl SimTime {
    /// Build a `SimTime` formatter that reads from the given layer handle.
    pub fn new(handle: SimulationLayerHandle) -> Self {
        Self { handle }
    }
}

impl FormatTime for SimTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let ms = self.handle.current_sim_time_ms();
        let secs = ms / 1000;
        let frac = ms % 1000;
        write!(w, "sim+{:>5}.{:03}s", secs, frac)
    }
}
