//! The determinism canary end to end (`SimulationBuilder::check_determinism`).
//!
//! A seed is run twice; the first run records a fingerprint after every draw
//! on the simulation stream and the second must reproduce the whole sequence.
//! A workload whose behavior depends on something the seed does not control —
//! here a process-wide counter that survives from the first run into the
//! second — trips it, whether it draws one extra time (the streams diverge)
//! or one time fewer (a matching prefix, then an early exit).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, NetworkProvider, Process,
    SimContext, SimulationBuilder, SimulationReport, SimulationResult, TcpListenerTrait,
    TimeProvider, Workload,
};

const CANARY: &str = "determinism canary: replay matched the recorded draw sequence";

fn canary_pass_count(report: &SimulationReport) -> u64 {
    report
        .assertion_details
        .iter()
        .find(|d| d.msg == CANARY)
        .map_or(0, |d| d.pass_count)
}

fn canary_violated(report: &SimulationReport) -> bool {
    report
        .assertion_violations
        .iter()
        .any(|violation| violation.contains(CANARY))
}

/// Draws and sleeps a few times; everything it does derives from the seed.
struct Honest;

#[async_trait]
impl Workload for Honest {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..8 {
            let delay = moonpool_sim::sim_random_range(1..20u64);
            let _ = ctx.time().sleep(Duration::from_millis(delay)).await;
        }
        Ok(())
    }
}

/// Echoes one message per connection until shutdown.
struct Echo;

#[async_trait]
impl Process for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            let accepted = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            if let Ok((mut stream, _)) = accepted {
                let mut buf = [0_u8; 8];
                if let Ok(n) = stream.read(&mut buf).await
                    && n > 0
                {
                    let _ = stream.write_all(&buf[..n]).await;
                }
            }
        }
    }
}

/// Talks to random echo processes under every fault the run draws; every
/// step is bounded by a simulated timeout so no fault can wedge it, and every
/// outcome is tolerated — the point is the draws, not the replies.
struct Chatter;

#[async_trait]
impl Workload for Chatter {
    fn name(&self) -> &'static str {
        "chatter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let peers = ctx.topology().all_process_ips();
        for _ in 0..40 {
            let peer = &peers[moonpool_sim::sim_random_range(0..peers.len())];
            let exchange = async {
                let mut stream = ctx.network().connect(peer).await?;
                let payload = [moonpool_sim::sim_random::<u8>(); 4];
                stream.write_all(&payload).await?;
                let mut reply = [0_u8; 4];
                stream.read_exact(&mut reply).await?;
                Ok::<_, std::io::Error>(())
            };
            let _ = ctx
                .time()
                .timeout(Duration::from_millis(500), exchange)
                .await;
            let pause = moonpool_sim::sim_random_range(10..200u64);
            let _ = ctx.time().sleep(Duration::from_millis(pause)).await;
        }
        Ok(())
    }
}

/// The nth run of the process draws `base + n % 2` times: the record run
/// (n = 0) and the replay (n = 1) disagree by exactly one draw.
struct Leaky {
    runs: &'static AtomicUsize,
    /// `+1` draws one extra time on the replay, `-1` one time fewer.
    delta: i32,
}

#[async_trait]
impl Workload for Leaky {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        let base = 6;
        let draws = if run % 2 == 1 {
            usize::try_from(i32::try_from(base).expect("small") + self.delta).expect("positive")
        } else {
            base
        };
        for _ in 0..draws {
            let delay = moonpool_sim::sim_random_range(1..20u64);
            let _ = ctx.time().sleep(Duration::from_millis(delay)).await;
        }
        Ok(())
    }
}

#[test]
fn honest_workload_passes_the_canary_on_every_seed() {
    let report = SimulationBuilder::new()
        .check_determinism()
        .set_iterations(5)
        .set_debug_seeds(vec![1, 2, 3, 4, 5])
        .workload_factory(|| Box::new(Honest))
        .run();

    assert_eq!(report.failed_runs, 0, "{:?}", report.assertion_violations);
    assert_eq!(report.iterations, 5);
    assert_eq!(canary_pass_count(&report), 5, "one verdict per seed");
    assert!(!canary_violated(&report));
}

/// Every fault family the simulator has — swarmed network and storage faults,
/// process attrition of every reboot kind, buggify, the operation-alphabet
/// swarm — is a draw on the one stream, so a chaotic seed must replay
/// fingerprint for fingerprint. This is the canary's standing proof that no
/// fault path keeps randomness of its own.
#[test]
fn canary_is_green_under_full_chaos() {
    let report = SimulationBuilder::new()
        .check_determinism()
        .processes(3, || Box::new(Echo))
        .workload_factory(|| Box::new(Chatter))
        .swarm_operations()
        .enable_chaos([
            Chaos::Network(ChaosMode::Swarm),
            Chaos::Storage(ChaosMode::Swarm),
            Chaos::Attrition {
                config: Attrition {
                    max_dead: 1,
                    prob_graceful: 0.3,
                    prob_crash: 0.5,
                    prob_wipe: 0.2,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                    victims: AttritionVictims::Any,
                },
                mode: ChaosMode::Swarm,
            },
        ])
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(8)
        .set_debug_seeds((1..=8).collect())
        .run();

    assert_eq!(report.failed_runs, 0, "{:?}", report.assertion_violations);
    assert_eq!(report.iterations, 8);
    assert_eq!(canary_pass_count(&report), 8, "one verdict per seed");
    assert!(!canary_violated(&report));
}

#[test]
fn extra_draw_on_the_replay_trips_the_canary() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let report = SimulationBuilder::new()
        .check_determinism()
        .set_iterations(1)
        .set_debug_seeds(vec![11])
        .workload_factory(|| {
            Box::new(Leaky {
                runs: &RUNS,
                delta: 1,
            })
        })
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(report.failed_runs, 1, "the seed must be reported as failed");
    assert_eq!(report.seeds_failing, vec![11]);
    assert!(
        canary_violated(&report),
        "{:?}",
        report.assertion_violations
    );
}

#[test]
fn early_exit_after_a_matching_prefix_trips_the_canary() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let report = SimulationBuilder::new()
        .check_determinism()
        .set_iterations(1)
        .set_debug_seeds(vec![12])
        .workload_factory(|| {
            Box::new(Leaky {
                runs: &RUNS,
                delta: -1,
            })
        })
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(report.failed_runs, 1, "a shorter replay must fail too");
    assert_eq!(report.seeds_failing, vec![12]);
    assert!(
        canary_violated(&report),
        "{:?}",
        report.assertion_violations
    );
}

#[test]
#[should_panic(expected = "check_determinism runs a seed more than once")]
fn instance_workloads_are_rejected() {
    let _ = SimulationBuilder::new()
        .check_determinism()
        .set_iterations(1)
        .workload(Honest)
        .run();
}
