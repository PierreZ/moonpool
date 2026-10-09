//! The simulator's side of [`moonpool_buggify::hint`]: a moment the code
//! under test names becomes a fault only when the seed's own chaos allows
//! one.
//!
//! A [`hint!`](moonpool_buggify::hint) point that fires asks the
//! [`HintSink`]. The sink finds the calling process by the task being
//! polled (the executor's task owner), then hands the decision to the first
//! attrition regime whose victim filter admits it. That regime's `max_dead`
//! budget, reboot-kind weights and recovery delays apply, exactly as for its
//! own timed reboots, so a seed whose swarm drew the never-reboot regime
//! never reboots on a hint either. Every hint maps to a reboot for now: the
//! one fault that tests a storage moment and a network moment alike.
//!
//! The harness may add one more veto, a budget of its own: a [`HintVeto`]
//! published in the run's state. The sink asks it last, once a reboot fits
//! the regime, so a yes is a reboot.

use std::sync::Arc;

use moonpool_buggify::hint::{Sink, Strike};

use crate::chaos::state_handle::StateHandle;
use crate::runner::fault_injector::{AttritionInjector, FaultContext};
use crate::{assert_reachable, assert_sometimes_each};

/// The run's state key a [`HintVeto`] is published under.
pub const HINT_VETO_KEY: &str = "moonpool:hint-veto";

/// A harness's budget over the reboots hints ask for.
///
/// The sink asks the veto with the process IP and the hint's label after
/// the attrition regime admits the reboot, and only then: a `true` answer
/// is a reboot that is scheduled at once, so the harness may record it as
/// done. A `false` answer leaves the process alive and the hint resolves.
///
/// Use it for a budget the simulator cannot see, such as the copies a
/// replicated store can lose before a crash is no longer survivable. The
/// veto must not draw randomness: it runs inside the hint's call.
#[derive(Clone)]
pub struct HintVeto(Arc<Permits>);

/// What a [`HintVeto`] asks: may the process at this IP reboot at this hint.
type Permits = dyn Fn(&str, &'static str) -> bool + Send + Sync;

impl HintVeto {
    /// A veto answering `permits(ip, label)`.
    pub fn new(permits: impl Fn(&str, &'static str) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(permits))
    }

    /// Install this veto for the run that owns `state`, replacing any
    /// earlier one.
    pub fn publish(self, state: &StateHandle) {
        state.publish(HINT_VETO_KEY, self);
    }

    fn permits(&self, ip: &str, label: &'static str) -> bool {
        (self.0)(ip, label)
    }
}

impl std::fmt::Debug for HintVeto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HintVeto")
    }
}

/// Strikes a process at its own hint, under the seed's attrition regimes.
pub(crate) struct HintSink {
    ctx: FaultContext,
    regimes: Vec<AttritionInjector>,
}

impl HintSink {
    /// A sink deciding through `regimes`, the run's resolved attrition
    /// regimes in registration order.
    pub(crate) fn new(ctx: FaultContext, regimes: Vec<AttritionInjector>) -> Self {
        Self { ctx, regimes }
    }
}

impl Sink for HintSink {
    fn strike(&self, label: &'static str) -> Strike {
        let Some(owner) = crate::executor::current_owner() else {
            assert_reachable!("hint: a hint outside a process is ignored");
            return Strike::None;
        };
        let ip = owner.to_string();
        let Some(regime) = self
            .regimes
            .iter()
            .find(|regime| regime.admits(&self.ctx, &ip))
        else {
            assert_reachable!("hint: no attrition regime covers the hinting process");
            return Strike::None;
        };
        let veto = self.ctx.state().get::<HintVeto>(HINT_VETO_KEY);
        let permit = || {
            let permitted = veto.as_ref().is_none_or(|veto| veto.permits(&ip, label));
            if !permitted {
                assert_reachable!("hint: the harness's veto refuses a reboot");
            }
            permitted
        };
        let strike = match regime.reboot_on_hint(&self.ctx, &ip, permit) {
            Ok(true) => Strike::Killed,
            Ok(false) => Strike::None,
            Err(error) => {
                tracing::warn!(%error, %ip, label, "hint not taken");
                Strike::None
            }
        };
        if strike == Strike::Killed {
            tracing::info!(%ip, label, "hint: rebooting the process at its own hint");
            assert_reachable!("hint: a process rebooted at its own hint");
        }
        assert_sometimes_each!(
            "hint_struck",
            [("killed", i64::from(strike == Strike::Killed))]
        );
        strike
    }
}
