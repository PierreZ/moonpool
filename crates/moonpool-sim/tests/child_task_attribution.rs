//! Regression: events from provider-spawned child tasks keep their actor.
//!
//! The observability layer attributes an event to the `ip` of the nearest
//! enclosing `process` / `workload` span and drops events that have none.
//! Root actors are instrumented with that span by the orchestrator, but a
//! task spawned through `ctx.task().spawn_task()` used to be handed to the
//! executor bare, so its polls never entered the actor span and every
//! correctness fact a background handler emitted vanished from the invariant
//! timeline.

use std::time::Duration;

use async_trait::async_trait;

use moonpool_sim::{
    Process, SimContext, SimulationBuilder, SimulationError, SimulationResult, TaskProvider,
    TimeProvider, TraceQuery, Workload,
};

const ROOT_EVENT: &str = "root_event";
const CHILD_EVENT: &str = "child_event";
const GRANDCHILD_EVENT: &str = "grandchild_event";

/// Emits one event from its root future, one from a spawned child, and one
/// from a child of that child.
struct SpawningProcess;

#[async_trait]
impl Process for SpawningProcess {
    fn name(&self) -> &'static str {
        "spawning_process"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        tracing::info!(target: "attribution", "root_event");
        let task = ctx.task().clone();
        let handle = ctx.task().spawn_task("child", async move {
            tracing::info!(target: "attribution", "child_event");
            let grandchild = task.spawn_task("grandchild", async move {
                tracing::info!(target: "attribution", "grandchild_event");
            });
            let _ = grandchild.await;
        });
        let _ = handle.await;
        // Stay alive so the run has a server for its whole duration.
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Waits for the process to have done its work, then reads the timeline.
struct TimelineWorkload;

#[async_trait]
impl Workload for TimelineWorkload {
    fn name(&self) -> &'static str {
        "timeline_workload"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server = ctx
            .topology()
            .all_process_ips()
            .first()
            .cloned()
            .ok_or_else(|| SimulationError::InvalidState("no server".to_string()))?;
        let q: &dyn TraceQuery = ctx.observability();
        for name in [ROOT_EVENT, CHILD_EVENT, GRANDCHILD_EVENT] {
            let events = q.snapshot(name);
            if events.len() != 1 {
                return Err(SimulationError::InvalidState(format!(
                    "{name}: expected one captured event, found {}",
                    events.len()
                )));
            }
            if events[0].source != server {
                return Err(SimulationError::InvalidState(format!(
                    "{name}: attributed to {:?}, expected the server {server:?}",
                    events[0].source
                )));
            }
        }
        Ok(())
    }
}

#[test]
fn events_from_spawned_child_tasks_are_attributed_to_their_process() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(SpawningProcess))
        .workload(TimelineWorkload)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(
        report.failed_runs, 0,
        "every event, root or spawned, must reach the timeline with its actor"
    );
}
