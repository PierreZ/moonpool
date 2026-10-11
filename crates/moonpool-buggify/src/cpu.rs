//! CPU charges: the code under test says it just did a known amount of
//! work, and a simulator with a CPU model makes that work take time.
//!
//! ```
//! use std::time::Duration;
//!
//! fn fold_batch(records: &[Vec<u8>]) {
//!     // ... the real work ...
//!     moonpool_buggify::cpu_charge(Duration::from_nanos(200) * records.len() as u32);
//! }
//! ```
//!
//! Put a charge only on a hot path a profile shows: an encode or decode of a
//! batch, a checksum, a fold. A simulated task poll costs a small base cost
//! already; the explicit charges carry the signal.
//!
//! Like [`hint!`](crate::hint), a charge is **inert** outside a simulation:
//! no sink is installed, so the call does nothing and costs one thread-local
//! read. A simulator installs its sink with [`set_sink`] only while its CPU
//! model is on.

use std::cell::RefCell;
use std::time::Duration;

/// The simulator's side of the charges, installed per thread for a run.
pub trait Sink {
    /// The task being polled did `cost` of CPU work.
    fn charge(&self, cost: Duration);
}

thread_local! {
    static SINK: RefCell<Option<Box<dyn Sink>>> = RefCell::new(None);
}

/// Install the simulator's sink on this thread. Called by the simulation
/// runtime when its CPU model is on.
pub fn set_sink(sink: Box<dyn Sink>) {
    SINK.with(|slot| *slot.borrow_mut() = Some(sink));
}

/// Remove the sink: every charge is inert again.
pub fn clear_sink() {
    SINK.with(|slot| *slot.borrow_mut() = None);
}

/// Report that the calling code just did `cost` of CPU work.
///
/// Inert outside a simulation with a CPU model. Inside one, the calling
/// process's clock moves forward by `cost` times its slowness, so what the
/// process does next (a send, a write, a timer) happens after the work.
pub fn cpu_charge(cost: Duration) {
    SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink.charge(cost);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Total(Rc<Cell<Duration>>);

    impl Sink for Total {
        fn charge(&self, cost: Duration) {
            self.0.set(self.0.get() + cost);
        }
    }

    #[test]
    fn a_charge_without_a_sink_does_nothing() {
        clear_sink();
        cpu_charge(Duration::from_secs(1));
    }

    #[test]
    fn a_charge_reaches_the_installed_sink_until_cleared() {
        let total = Rc::new(Cell::new(Duration::ZERO));
        set_sink(Box::new(Total(Rc::clone(&total))));
        cpu_charge(Duration::from_micros(3));
        cpu_charge(Duration::from_micros(4));
        clear_sink();
        cpu_charge(Duration::from_micros(100));
        assert_eq!(total.get(), Duration::from_micros(7));
    }
}
