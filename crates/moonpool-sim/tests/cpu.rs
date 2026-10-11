//! The CPU model (`Chaos::Cpu`) and gray failures (`Chaos::GrayFailure`)
//! through the builder.
//!
//! Echo servers answer a pinging workload while they sample their own
//! counters. With the model on, the servers' CPU time is real and in range,
//! the campaign replays bit for bit, and the swarm leaves the model off on
//! some seeds. A gray failure slows at most its budget of machines at once,
//! reaches each kind, and leaves every machine healthy at the cutoff.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{Providers, SystemProvider, SystemSample};
use moonpool_sim::{
    Chaos, ChaosMode, FaultContext, FaultInjector, GrayFailure, NetworkProvider, Process,
    SimContext, SimWorld, SimulationBuilder, SimulationError, SimulationReport, SimulationResult,
    Slowness, TcpListenerTrait, TimeProvider, Workload,
};

const CHAOS: Duration = Duration::from_secs(4);

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(shared: &Shared<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().expect("Mutex poisoned: prior task panicked")
}

fn sleep_failed(error: &moonpool_core::TimeError) -> SimulationError {
    SimulationError::InvalidState(format!("sleep failed: {error}"))
}

/// Echoes one connection at a time, and samples its own counters every
/// 100 ms.
struct Echo {
    samples: Shared<Vec<SystemSample>>,
}

impl Echo {
    async fn sample_forever(ctx: &SimContext, samples: Shared<Vec<SystemSample>>) {
        loop {
            if ctx.time().sleep(Duration::from_millis(100)).await.is_err() {
                return;
            }
            if let Ok(sample) = ctx.providers().system().sample("") {
                lock(&samples).push(sample);
            }
        }
    }

    async fn serve(ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buf = [0_u8; 8];
            while stream.read_exact(&mut buf).await.is_ok() {
                if stream.write_all(&buf).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[async_trait]
impl Process for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::select! {
            biased;
            () = ctx.shutdown().cancelled() => Ok(()),
            () = Self::sample_forever(ctx, self.samples.clone()) => Ok(()),
            result = Self::serve(ctx) => result,
        }
    }
}

/// Pings every server, in turn, until the chaos window is over. Every call
/// has a timeout: a slow machine is late, never a reason to hang.
struct Pinger;

impl Pinger {
    async fn ping(ctx: &SimContext, server: &str) {
        let patience = Duration::from_secs(1);
        let Ok(Ok(mut stream)) = ctx
            .time()
            .timeout(patience, ctx.network().connect(server))
            .await
        else {
            return;
        };
        for round in 0..5_u64 {
            let mut buf = [0_u8; 8];
            let exchange = async {
                stream.write_all(&round.to_le_bytes()).await?;
                stream.read_exact(&mut buf).await
            };
            if !matches!(ctx.time().timeout(patience, exchange).await, Ok(Ok(()))) {
                return;
            }
        }
    }
}

#[async_trait]
impl Workload for Pinger {
    fn name(&self) -> &'static str {
        "pinger"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Let every server bind first.
        ctx.time()
            .sleep(Duration::from_millis(200))
            .await
            .map_err(|error| sleep_failed(&error))?;
        let started = ctx.time().now();
        let servers = ctx.topology().all_process_ips().to_vec();
        while ctx.time().now().saturating_sub(started) < CHAOS + Duration::from_secs(1) {
            for server in &servers {
                Self::ping(ctx, server).await;
            }
            ctx.time()
                .sleep(Duration::from_millis(50))
                .await
                .map_err(|error| sleep_failed(&error))?;
        }
        Ok(())
    }
}

fn echo(samples: &Shared<Vec<SystemSample>>) -> impl Fn() -> Box<dyn Process> + 'static {
    let samples = samples.clone();
    move || {
        Box::new(Echo {
            samples: samples.clone(),
        })
    }
}

fn campaign(
    chaos: Vec<Chaos>,
    seeds: Vec<u64>,
    samples: &Shared<Vec<SystemSample>>,
) -> SimulationBuilder {
    SimulationBuilder::new()
        .processes(3, echo(samples))
        .workload_factory(|| Box::new(Pinger))
        .enable_chaos(chaos)
        .chaos_duration(CHAOS)
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds)
}

fn reached(report: &SimulationReport, message: &str) -> bool {
    report
        .assertion_results
        .get(message)
        .is_some_and(|stats| stats.successes > 0)
}

#[test]
fn the_cpu_model_counts_real_cpu_time_and_replays() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let report = campaign(vec![Chaos::Cpu(ChaosMode::Random)], vec![11, 12], &samples)
        .check_determinism()
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let samples = lock(&samples).clone();
    assert!(!samples.is_empty());
    assert!(
        samples
            .iter()
            .any(|sample| sample.process_cpu > Duration::ZERO),
        "the servers used CPU time"
    );
    for sample in &samples {
        assert!(sample.process_cpu <= sample.at, "one core: {sample:?}");
        assert_eq!(Some(sample.process_cpu), sample.run_loop_busy);
        assert_eq!(sample.process_cpu, sample.machine_busy);
    }
}

#[test]
fn without_the_cpu_model_a_poll_costs_nothing() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let report = campaign(Vec::new(), vec![11], &samples)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let samples = lock(&samples).clone();
    assert!(!samples.is_empty());
    assert!(
        samples
            .iter()
            .all(|sample| sample.process_cpu == Duration::ZERO)
    );
}

#[test]
fn the_swarm_turns_the_cpu_model_on_and_off() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let report = campaign(
        vec![Chaos::Cpu(ChaosMode::Swarm)],
        (1..=8).collect(),
        &samples,
    )
    .run()
    .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(reached(&report, "cpu: a seed runs with the CPU model on"));
    assert!(reached(&report, "cpu: a seed runs with the CPU model off"));
}

/// What the probe saw: the most machines slow at once, and every machine
/// ever slowed.
#[derive(Debug, Default, Clone)]
struct GrayProbe {
    most_at_once: usize,
    slowed: BTreeSet<String>,
}

/// Watches every machine's slowness through the chaos window.
struct GrayWatch(Shared<GrayProbe>);

#[async_trait]
impl FaultInjector for GrayWatch {
    fn name(&self) -> &'static str {
        "gray_watch"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        while !ctx.chaos_shutdown().is_cancelled() {
            let mut slow = Vec::new();
            for ip in ctx.process_ips() {
                if !ctx.slowness(ip)?.is_healthy() {
                    slow.push(ip.clone());
                }
            }
            {
                let mut probe = lock(&self.0);
                probe.most_at_once = probe.most_at_once.max(slow.len());
                probe.slowed.extend(slow);
            }
            ctx.time()
                .sleep(Duration::from_millis(10))
                .await
                .map_err(|error| sleep_failed(&error))?;
        }
        Ok(())
    }
}

#[test]
fn a_gray_failure_slows_within_its_budget_and_reaches_every_kind() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let probe: Shared<GrayProbe> = Arc::default();
    let factory_probe = probe.clone();
    let report = campaign(
        vec![
            Chaos::Cpu(ChaosMode::Random),
            Chaos::GrayFailure {
                config: GrayFailure::new(1),
                mode: ChaosMode::Random,
            },
        ],
        (1..=4).collect(),
        &samples,
    )
    .fault_factory(move || Box::new(GrayWatch(factory_probe.clone())))
    .check_determinism()
    .run()
    .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let probe = lock(&probe).clone();
    assert_eq!(probe.most_at_once, 1, "the budget is one slow machine");
    assert!(!probe.slowed.is_empty());
    for kind in ["gray: cpu slow", "gray: disk slow", "gray: network slow"] {
        assert!(reached(&report, kind), "{kind} never reached");
    }
}

#[test]
fn a_gray_failure_never_slows_a_cpu_without_the_model() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let report = campaign(
        vec![Chaos::GrayFailure {
            config: GrayFailure::new(2),
            mode: ChaosMode::Random,
        }],
        (1..=4).collect(),
        &samples,
    )
    .run()
    .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(!reached(&report, "gray: cpu slow"));
    assert!(reached(&report, "gray: disk slow"));
}

#[test]
fn a_gray_failure_names_only_registered_groups() {
    let samples: Shared<Vec<SystemSample>> = Arc::default();
    let result = campaign(
        vec![Chaos::GrayFailure {
            config: GrayFailure::new(1).victims(moonpool_sim::AttritionVictims::group("acceptor")),
            mode: ChaosMode::Random,
        }],
        vec![1],
        &samples,
    )
    .run();
    assert!(matches!(
        result,
        Err(SimulationError::InvalidConfiguration(_))
    ));
}

#[test]
fn recovery_mode_makes_every_machine_healthy() {
    let mut sim = SimWorld::new();
    let ip: IpAddr = "10.0.1.1".parse().expect("valid IP");
    let slow = Slowness {
        cpu: 50,
        disk: 10,
        network: 3,
    };
    sim.set_slowness(ip, slow);
    assert_eq!(sim.slowness(ip), slow);
    sim.enter_recovery_mode();
    assert!(sim.slowness(ip).is_healthy());
}
