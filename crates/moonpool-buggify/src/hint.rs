//! Hints: the code under test names an interesting moment, and the
//! simulator decides what, if anything, goes wrong there.
//!
//! ```
//! use moonpool_buggify::hint;
//!
//! async fn persist_then_send(/* storage, messages */) {
//!     // storage.sync().await?;
//!     hint!("batch durable, not sent").await;
//!     // send(messages).await;
//! }
//! ```
//!
//! The batch is durable, its messages are not sent yet: a crash right here
//! leaves the peers not knowing what this process holds. The code names the
//! moment, never a fault. The simulator chooses: whether to act, which
//! fault (from the families the seed's own chaos enabled), and how hard.
//! `FoundationDB`'s server code does the same with `if (buggify()) throw
//! please_reboot()`, which its simulated worker turns into a reboot.
//!
//! Three vetoes stand between a hint and a fault; none of them commands:
//!
//! 1. The point is a disruptive buggify location of its own: activated once
//!    per run, firing at a low rate per call ([`POINT_PROB`], or a rate
//!    literal at the site), and silent once the run entered its recovery
//!    tail.
//! 2. The seed's chaos: a seed whose regime never reboots never reboots.
//! 3. The budget: a regime's limit on dead processes still applies.
//!
//! Like buggify, a hint is **inert** outside a simulation: buggify is
//! disabled, so the point draws nothing, asks no one, and the returned
//! future resolves at once.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// What the simulator did with a hint that fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strike {
    /// Nothing: no fault fits, the seed's chaos forbids one, or the caller
    /// is no process (a workload, an injector, the driver).
    None,
    /// A fault the caller survives and goes on through.
    Struck,
    /// The calling process is killed: the kill is scheduled, and the
    /// caller's future never resolves.
    Killed,
}

/// The simulator's side of the hints, installed per thread for a run.
pub trait Sink {
    /// The calling process reached the moment `label`, and its point fired.
    fn strike(&self, label: &'static str) -> Strike;
}

thread_local! {
    static SINK: RefCell<Option<Box<dyn Sink>>> = RefCell::new(None);
}

/// Install the simulator's sink on this thread. Called by the simulation
/// runtime for the chaos window of a run.
pub fn set_sink(sink: Box<dyn Sink>) {
    SINK.with(|slot| *slot.borrow_mut() = Some(sink));
}

/// Remove the sink: every hint is inert again.
pub fn clear_sink() {
    SINK.with(|slot| *slot.borrow_mut() = None);
}

/// The default firing rate of an active [`hint!`](crate::hint) point, per
/// call. Low, because a point on a hot path is reached often.
pub const POINT_PROB: f64 = 0.05;

/// Report the moment `label` at `location`, firing at `prob` per call while
/// the location is active. Use [`hint!`](crate::hint), which names the
/// location.
///
/// The hint is taken when this function is called, not when the future is
/// polled. If the sink kills the caller, the returned future never resolves:
/// the kill lands within one scheduler tick and aborts the task, and every
/// write not yet synced resolves by crash physics. Otherwise the future
/// resolves at once.
#[must_use = "await the hint, so no code after the moment runs once the process is killed"]
pub fn at(label: &'static str, prob: f64, location: &'static str) -> Hinted {
    let strike = if crate::buggify_fault_internal(prob, location) {
        SINK.with(|slot| {
            slot.borrow()
                .as_ref()
                .map_or(Strike::None, |sink| sink.strike(label))
        })
    } else {
        Strike::None
    };
    Hinted { strike }
}

/// The future [`at`] returns: ready unless the strike killed the caller.
#[derive(Debug)]
pub struct Hinted {
    strike: Strike,
}

impl Hinted {
    /// What the simulator did at this point.
    #[must_use]
    pub fn strike(&self) -> Strike {
        self.strike
    }
}

impl Future for Hinted {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.strike == Strike::Killed {
            // The scheduled kill aborts this task; nothing wakes it.
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Waker;

    struct Recording {
        labels: Rc<RefCell<Vec<&'static str>>>,
        answer: Strike,
    }

    impl Sink for Recording {
        fn strike(&self, label: &'static str) -> Strike {
            self.labels.borrow_mut().push(label);
            self.answer
        }
    }

    /// A sink that only records that it was asked.
    struct Flag(Rc<Cell<bool>>);

    impl Sink for Flag {
        fn strike(&self, _label: &'static str) -> Strike {
            self.0.set(true);
            Strike::Killed
        }
    }

    fn poll_once(mut future: Hinted) -> Poll<()> {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(&mut future).poll(&mut cx)
    }

    /// A source that always draws 0.0: every location activates and fires.
    fn zero() -> f64 {
        0.0
    }

    /// Run `f` with buggify enabled on the always-firing source and a sink
    /// that answers `answer`; returns the labels the sink was asked about.
    fn with_sink(answer: Strike, f: impl FnOnce()) -> Vec<&'static str> {
        let labels = Rc::new(RefCell::new(Vec::new()));
        crate::set_random_source(zero);
        crate::buggify_init(1.0);
        set_sink(Box::new(Recording {
            labels: Rc::clone(&labels),
            answer,
        }));
        f();
        clear_sink();
        crate::buggify_reset();
        crate::clear_random_source();
        labels.take()
    }

    #[test]
    fn outside_a_simulation_a_hint_resolves_and_asks_no_one() {
        crate::buggify_reset();
        let asked = Rc::new(Cell::new(false));
        set_sink(Box::new(Flag(Rc::clone(&asked))));
        assert_eq!(poll_once(crate::hint!("idle")), Poll::Ready(()));
        assert!(!asked.get(), "a disabled buggify asks no sink");
        clear_sink();
    }

    #[test]
    fn a_kill_never_resolves() {
        let labels = with_sink(Strike::Killed, || {
            let future = crate::hint!("batch durable, not sent");
            assert_eq!(future.strike(), Strike::Killed, "taken at the call");
            assert_eq!(poll_once(future), Poll::Pending);
        });
        assert_eq!(labels, ["batch durable, not sent"]);
    }

    #[test]
    fn a_survivable_strike_or_none_resolves() {
        for answer in [Strike::Struck, Strike::None] {
            let labels = with_sink(answer, || {
                assert_eq!(poll_once(crate::hint!("staged", 1.0)), Poll::Ready(()));
            });
            assert_eq!(labels, ["staged"]);
        }
    }

    #[test]
    fn a_hint_is_silent_in_the_recovery_tail() {
        let labels = with_sink(Strike::Killed, || {
            crate::buggify_enter_recovery();
            assert_eq!(poll_once(crate::hint!("late")), Poll::Ready(()));
        });
        assert!(labels.is_empty(), "the sink is never asked once recovering");
    }
}
