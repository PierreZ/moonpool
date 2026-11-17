//! `KillPolicy`: the harness's `canKillProcesses`. Every kill and wipe the
//! fault context runs asks the published policy first: `KeepDisk` turns a
//! wipe into a crash, `Restart` reboots the victim in place with no crash, and the ask names the
//! victim, the kind, whether it is held down, and who is dead and wiped.

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, FaultContext, FaultInjector,
    Invariant, KillAsk, KillPolicy, KillVerdict, Process, RebootKind, SIM_FAULT_EVENT_NAME,
    SimContext, SimulationBuilder, SimulationError, SimulationResult, TimeProvider, TraceQuery,
    Workload,
};

/// Every ask the policy saw.
type Asks = Arc<Mutex<Vec<KillAsk>>>;

/// Every force-kill's cause, in order.
type Causes = Arc<Mutex<Vec<String>>>;

/// A server process that publishes the policy at each boot and idles.
struct Node {
    verdict: KillVerdict,
    asks: Asks,
}

#[async_trait]
impl Process for Node {
    fn name(&self) -> &'static str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let verdict = self.verdict;
        let asks = self.asks.clone();
        KillPolicy::new(move |_state, ask| {
            asks.lock()
                .expect("Mutex poisoned: prior task panicked")
                .push(ask.clone());
            verdict
        })
        .publish(ctx.state());
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Keeps the run alive past the chaos window.
struct Timed(Duration);

#[async_trait]
impl Workload for Timed {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(self.0).await;
        Ok(())
    }
}

/// Records the cause of every force-kill from the fault timeline.
struct Kills {
    cursor: Cell<usize>,
    causes: Causes,
}

impl Invariant for Kills {
    fn name(&self) -> &'static str {
        "kills"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut causes = self.causes.lock().expect("causes lock");
        for event in q.since(SIM_FAULT_EVENT_NAME, &self.cursor) {
            if event.str("kind") == Some("process_force_kill") {
                causes.push(event.str("cause").unwrap_or("").to_string());
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
    }
}

/// Three nodes under an attrition that only wipes, judged by a policy that
/// answers `verdict`: what the policy was asked and the causes of the kills.
fn wipes_under(verdict: KillVerdict) -> (Vec<KillAsk>, Vec<String>) {
    let asks: Asks = Arc::default();
    let causes: Causes = Arc::default();
    let node_asks = asks.clone();
    let report = SimulationBuilder::new()
        .processes(3, move || {
            Box::new(Node {
                verdict,
                asks: node_asks.clone(),
            })
        })
        .workload(Timed(Duration::from_secs(30)))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.0,
                prob_crash: 0.0,
                prob_wipe: 1.0,
                recovery_delay_ms: Some(200..600),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::group("node"),
                wipe_budget: None,
            },
            mode: ChaosMode::Random,
        }])
        .invariant(Kills {
            cursor: Cell::new(0),
            causes: causes.clone(),
        })
        .chaos_duration(Duration::from_secs(20))
        .set_iterations(2)
        .set_debug_seeds(vec![3, 5])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let asks = asks.lock().expect("asks lock").clone();
    let causes = causes.lock().expect("causes lock").clone();
    (asks, causes)
}

#[test]
fn an_allowing_policy_lets_every_wipe_run() {
    let (asks, causes) = wipes_under(KillVerdict::Allow);
    assert!(!asks.is_empty());
    assert!(asks.iter().all(KillAsk::wipes), "{asks:?}");
    assert!(
        causes.iter().any(|cause| cause == "crash_and_wipe"),
        "{causes:?}"
    );
}

#[test]
fn keep_disk_turns_every_wipe_into_a_crash() {
    let (asks, causes) = wipes_under(KillVerdict::KeepDisk);
    assert!(!asks.is_empty());
    assert!(!causes.is_empty(), "the kills still run");
    assert!(causes.iter().all(|cause| cause == "crash"), "{causes:?}");
}

#[test]
fn restart_reboots_the_victim_in_place() {
    let (asks, causes) = wipes_under(KillVerdict::Restart);
    assert!(!asks.is_empty());
    assert!(!causes.is_empty(), "the reboots still run");
    assert!(
        causes.iter().all(|cause| cause == "restart_in_place"),
        "{causes:?}"
    );
}

/// Holds the first node down, then crashes the second for a while.
struct Script;

#[async_trait]
impl FaultInjector for Script {
    fn name(&self) -> &'static str {
        "script"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let sleep = |ms| async move {
            ctx.time()
                .sleep(Duration::from_millis(ms))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))
        };
        sleep(100).await?;
        let ips = ctx.process_ips();
        ctx.crash(&ips[0])?;
        sleep(50).await?;
        ctx.crash_for(&ips[1], Duration::from_millis(500))?;
        sleep(50).await?;
        ctx.restart(&ips[0])?;
        Ok(())
    }
}

#[test]
fn the_ask_names_the_victim_the_held_down_and_the_dead() {
    let asks: Asks = Arc::default();
    let node_asks = asks.clone();
    let report = SimulationBuilder::new()
        .processes(3, move || {
            Box::new(Node {
                verdict: KillVerdict::Allow,
                asks: node_asks.clone(),
            })
        })
        .workload(Timed(Duration::from_secs(5)))
        .fault_factory(|| Box::new(Script))
        .chaos_duration(Duration::from_secs(2))
        .set_iterations(1)
        .set_debug_seeds(vec![7])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let asks = asks.lock().expect("asks lock").clone();
    assert_eq!(asks.len(), 2, "{asks:?}");
    let (held, timed) = (&asks[0], &asks[1]);
    assert!(held.held);
    assert_eq!(held.kind, RebootKind::Crash);
    assert!(held.dead.is_empty());
    assert!(!timed.held);
    assert_eq!(
        timed.dead.iter().copied().collect::<Vec<_>>(),
        [held.victim]
    );
    assert_ne!(timed.victim, held.victim);
    assert!(timed.wiped.is_empty());
}
