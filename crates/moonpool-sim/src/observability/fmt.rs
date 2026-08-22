//! `tracing_subscriber::fmt` integration that uses simulation time as the
//! timestamp prefix.
//!
//! By default, `tracing_subscriber::fmt::layer()` prints wall-clock time —
//! useless in simulation, where logical time skips idle periods and seconds
//! of simulated activity may take milliseconds of wall time. [`SimTime`]
//! prints "current sim time in milliseconds" reported by a [`Clock`] instead.
//!
//! [`Clock`] is the narrow `Send + Sync` source of sim time the formatter
//! needs. [`super::SimulationLayerHandle`] implements it; users can also
//! implement it for testing stubs or alternate time sources.
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
//! // Important: register `SimulationLayer` BEFORE the fmt layer so its
//! // on_event runs first and updates the layer's sim time before fmt
//! // formats the event.
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

/// Narrow `Send + Sync` source of simulation time, suitable for plugging into
/// the [`SimTime`] formatter.
///
/// Distinct from [`moonpool_core::TimeProvider`], which is `Send + Sync` but
/// not dyn-compatible (it has a generic `timeout` method). Anything that can
/// hand out a `u64` representing "current sim time in milliseconds" can
/// implement this — typically the active [`SimulationLayerHandle`], but
/// tests and alternate time sources can implement it too.
pub trait Clock: Send + Sync {
    /// Current simulation time in milliseconds.
    fn now_ms(&self) -> u64;
}

impl Clock for SimulationLayerHandle {
    fn now_ms(&self) -> u64 {
        self.current_sim_time_ms()
    }
}

/// `FormatTime` implementation that writes simulation time reported by a
/// [`Clock`] instead of wall-clock time.
///
/// Holds only a `Box<dyn Clock>` — the formatter does not see the rest of
/// the layer's API.
pub struct SimTime {
    clock: Box<dyn Clock>,
}

impl SimTime {
    /// Build a `SimTime` formatter that reads from the given clock.
    pub fn new<C: Clock + 'static>(clock: C) -> Self {
        Self {
            clock: Box::new(clock),
        }
    }
}

impl FormatTime for SimTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let ms = self.clock.now_ms();
        let secs = ms / 1000;
        let frac = ms % 1000;
        write!(w, "sim+{secs:>5}.{frac:03}s")
    }
}
