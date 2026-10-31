//! A live simulation: one seed, driven in steps by its caller.
//!
//! [`SimulationBuilder::run`] runs every iteration to completion and returns
//! a report. A live simulation runs one iteration of the same builder, but
//! the caller drives it: [`LiveSimulation::advance`] runs it until a stop
//! condition holds (a simulated time, say), then returns; the next call
//! resumes it where it stopped. Between two calls, the caller may change
//! what the workloads see (a queue of operator commands, for example), so a
//! person can act on a running simulation, as a browser page does.
//!
//! Pausing changes nothing in the run: the executor polls in the same order
//! as an uninterrupted run ([`Executor::drive`]), so a live run whose inputs
//! arrive at fixed simulated times is the same run as [`SimulationBuilder::run`]
//! with the same inputs.
//!
//! A live simulation has no exploration, no determinism canary and no
//! report: it is for watching a system, not for hunting bugs.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::SimulationError;
use crate::executor::Executor;
use crate::observability::{SimulationLayer, SimulationLayerHandle};
use crate::providers::TaskPanicTracker;
use crate::sim::WeakSimWorld;

use super::builder::SimulationBuilder;
use super::iteration::IterationManager;
use super::run_loop::OrchestrationOutcome;

/// Where a live simulation is after [`LiveSimulation::advance`].
#[derive(Debug)]
pub enum LiveStatus {
    /// The stop condition paused it: call `advance` again to go on.
    Running,
    /// Every workload returned and the run ended: `Ok` when every workload
    /// and invariant passed, the first error otherwise. Further calls to
    /// `advance` answer the same.
    Finished(Result<(), SimulationError>),
}

/// One iteration of a [`SimulationBuilder`], driven in steps.
///
/// Built by [`SimulationBuilder::live`]. Dropping it stops the run: the
/// executor cancels every task. The builder it came from is leaked (a few
/// kilobytes): the run borrows it for as long as it lives.
pub struct LiveSimulation {
    // Drop order matters: the run first, then its executor, then the
    // observability layer and the thread-local overrides.
    run: Option<Pin<Box<dyn Future<Output = OrchestrationOutcome>>>>,
    executor: Executor,
    clock: WeakSimWorld,
    task_panics: TaskPanicTracker,
    obs: SimulationLayerHandle,
    finished: Option<Result<(), String>>,
    _guards: LiveGuards,
}

/// The thread-local state a run installs, undone when the live run drops.
struct LiveGuards {
    _obs: crate::observability::InstallGuard,
}

impl Drop for LiveGuards {
    fn drop(&mut self) {
        crate::sim::uninstall_select_offset();
        crate::chaos::exploration_glue::cleanup_assertion_region();
    }
}

impl std::fmt::Debug for LiveSimulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSimulation")
            .field("now", &self.now())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl SimulationBuilder {
    /// Start one iteration of this simulation as a [`LiveSimulation`], the
    /// first seed of the builder's seeds (or a drawn one), and return before
    /// any of it runs.
    ///
    /// # Errors
    ///
    /// The builder's own checks ([`SimulationBuilder::run`]'s), and a
    /// builder with no workload or with exploration: a live run is one
    /// seed, watched.
    pub fn live(mut self) -> Result<LiveSimulation, SimulationError> {
        self.validate_rerun_lifecycle()?;
        self.validate_fault_injectors()?;
        if self.entries.is_empty() {
            return Err(SimulationError::InvalidState(
                "a live simulation needs a workload".into(),
            ));
        }
        if self.exploration_config.is_some() {
            return Err(SimulationError::InvalidState(
                "a live simulation does not explore".into(),
            ));
        }

        let layer = SimulationLayer::new()
            .with_level(self.trace_level)
            .with_capture(self.timeline == super::builder::TimelineCapture::On);
        let (obs, obs_guard) = layer.install();
        let guards = LiveGuards { _obs: obs_guard };
        for invariant in self.invariants.drain(..) {
            obs.register(invariant);
        }
        // Without exploration configured, no explorer is created.
        #[cfg(feature = "exploration")]
        drop(Self::init_assertions_and_exploration(None));
        #[cfg(not(feature = "exploration"))]
        Self::init_assertions_and_exploration(None);

        let mut iterations =
            IterationManager::new(self.iteration_control.clone(), self.seeds.clone());
        let seed = iterations.next_iteration();
        let iteration = iterations.current_iteration();
        self.prepare_iteration(&obs, seed, iteration);

        // The run borrows the builder for its whole life.
        let builder: &'static mut SimulationBuilder = Box::leak(Box::new(self));
        let mut return_map = Vec::new();
        let inputs = builder.orchestrator_inputs(&mut return_map, &obs, seed, iteration);
        let clock = inputs.sim().downgrade();
        let task_panics = TaskPanicTracker::default();
        let run = Box::pin(Self::orchestrator_future(inputs, task_panics.clone()));
        Ok(LiveSimulation {
            run: Some(run),
            executor: Executor::new(seed),
            clock,
            task_panics,
            obs,
            finished: None,
            _guards: guards,
        })
    }
}

impl LiveSimulation {
    /// The simulated time now, from the start of the run.
    #[must_use]
    pub fn now(&self) -> Duration {
        self.clock.now().unwrap_or_default()
    }

    /// Run until `stop` answers `true`, or until the run ends. `stop` is
    /// asked after each pass over the task pool, that is after each
    /// simulated event; it can read [`LiveSimulation::now`] through a
    /// clone of the clock, or any state the workloads share.
    ///
    /// # Panics
    ///
    /// As [`Executor::block_on`]: on a deadlock of the run.
    pub fn advance(&mut self, stop: &mut dyn FnMut() -> bool) -> LiveStatus {
        if let Some(finished) = &self.finished {
            return LiveStatus::Finished(finished.clone().map_err(SimulationError::InvalidState));
        }
        let Some(run) = self.run.as_mut() else {
            return LiveStatus::Finished(Ok(()));
        };
        let Some(outcome) = self.executor.drive(run.as_mut(), stop) else {
            return LiveStatus::Running;
        };
        self.run = None;
        let outcome =
            SimulationBuilder::finish_orchestration(outcome, &self.task_panics, &self.obs);
        let result = match outcome {
            Err((seeds, _)) => Err(format!("the run deadlocked (seeds {seeds:?})")),
            Ok(output) => output
                .results
                .into_iter()
                .find_map(Result::err)
                .map_or(Ok(()), |error| Err(error.to_string())),
        };
        let result = result.and_then(|()| {
            if crate::chaos::has_always_violations() {
                Err("an always assertion failed".to_string())
            } else {
                Ok(())
            }
        });
        self.finished = Some(result.clone());
        LiveStatus::Finished(result.map_err(SimulationError::InvalidState))
    }

    /// A clock that reads this run's simulated time, for a `stop` closure
    /// passed to [`LiveSimulation::advance`].
    #[must_use]
    pub fn clock(&self) -> LiveClock {
        LiveClock {
            world: self.clock.clone(),
        }
    }
}

/// A read-only handle on a live run's simulated time.
#[derive(Debug, Clone)]
pub struct LiveClock {
    world: WeakSimWorld,
}

impl LiveClock {
    /// The simulated time now; zero once the run is gone.
    #[must_use]
    pub fn now(&self) -> Duration {
        self.world.now().unwrap_or_default()
    }
}
