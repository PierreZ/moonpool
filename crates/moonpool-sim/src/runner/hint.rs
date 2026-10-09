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

use moonpool_buggify::hint::{Sink, Strike};

use crate::runner::fault_injector::{AttritionInjector, FaultContext};
use crate::{assert_reachable, assert_sometimes_each};

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
        let strike = match regime.reboot_on_hint(&self.ctx, &ip) {
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
