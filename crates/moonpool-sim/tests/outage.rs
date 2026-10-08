//! Correlated group outages (`Chaos::Outage`).
//!
//! Two groups, one outage over one of them: every victim is dead at one
//! instant while the other group is untouched, each victim restarts after
//! its own drawn delay with the straggler last (after the chaos window has
//! closed), the dead set drains back to empty before the run ends, and the
//! same seed replays the same kill and restart schedule.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Chaos, ChaosMode, FaultContext, FaultInjector, NetworkProvider, OUTAGE_STATE_KEY, Outage,
    OutageLanded, Process, SimContext, SimulationBuilder, SimulationError, SimulationResult,
    TcpListenerTrait, TimeProvider, Workload,
};

const CHAOS: Duration = Duration::from_secs(5);

/// Every boot: the process's IP and the simulated time it booted.
type Boots = Arc<Mutex<Vec<(String, Duration)>>>;

/// What the probe injector saw.
#[derive(Debug, Default, Clone, PartialEq)]
struct Probe {
    /// Every victim dead, and every process outside the outage alive, when
    /// the landing was published.
    all_down_at_once: Option<bool>,
    /// When the dead set drained back to empty.
    recovered_at: Option<Duration>,
    landed: Option<OutageLanded>,
}

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(shared: &Shared<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().expect("Mutex poisoned: prior task panicked")
}

/// Records its boot, binds a listener, and idles until shutdown.
struct Node {
    role: &'static str,
    boots: Boots,
}

#[async_trait]
impl Process for Node {
    fn name(&self) -> &str {
        self.role
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        lock(&self.boots).push((ctx.my_ip().to_string(), ctx.time().now()));
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            moonpool_sim::select! {
                biased;
                _ = listener.accept() => {}
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

/// Waits for the outage to land, checks who is dead at that instant, then
/// waits for every victim to be back.
struct ProbeInjector(Shared<Probe>);

#[async_trait]
impl FaultInjector for ProbeInjector {
    fn name(&self) -> &'static str {
        "outage_probe"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let tick = Duration::from_millis(1);
        let landed = loop {
            if let Some(landed) = ctx.state().get::<OutageLanded>(OUTAGE_STATE_KEY) {
                break landed;
            }
            ctx.time()
                .sleep(tick)
                .await
                .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
        };
        let all_down = landed.victims.iter().all(|ip| ctx.is_dead(ip))
            && ctx
                .ips_in_group("matchmaker")
                .iter()
                .all(|ip| !ctx.is_dead(ip))
            && ctx.dead_count() == landed.victims.len();
        {
            let mut probe = lock(&self.0);
            probe.all_down_at_once = Some(all_down);
            probe.landed = Some(landed);
        }
        // Keeps watching after the chaos window closes: the restarts are owed.
        while ctx.dead_count() > 0 {
            ctx.time()
                .sleep(tick)
                .await
                .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
        }
        lock(&self.0).recovered_at = Some(ctx.time().now());
        Ok(())
    }
}

/// Runs long enough for every restart, straggler included, to land.
struct Patience;

#[async_trait]
impl Workload for Patience {
    fn name(&self) -> &'static str {
        "patience"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_secs(20))
            .await
            .map_err(|e| SimulationError::InvalidState(e.to_string()))
    }
}

fn node(role: &'static str, boots: &Boots) -> impl Fn() -> Box<dyn Process> + 'static {
    let boots = boots.clone();
    move || {
        Box::new(Node {
            role,
            boots: boots.clone(),
        })
    }
}

fn outage() -> Outage {
    Outage {
        // Strikes near the end of the window, so restarts land after it.
        start: Duration::from_secs(4)..Duration::from_secs(5),
        down: Duration::from_secs(2)..Duration::from_secs(4),
        straggler: Some(Duration::from_secs(6)..Duration::from_secs(8)),
        ..Outage::groups(["acceptor"])
    }
}

/// One seed of the outage campaign: its boots and what the probe saw.
fn run_seed(seed: u64) -> (Vec<(String, Duration)>, Probe) {
    let boots: Boots = Arc::default();
    let probe: Shared<Probe> = Arc::default();
    let factory_probe = probe.clone();
    let report = SimulationBuilder::new()
        .processes(3, node("acceptor", &boots))
        .processes(2, node("matchmaker", &boots))
        .workload(Patience)
        .enable_chaos([Chaos::Outage {
            config: outage(),
            mode: ChaosMode::Random,
        }])
        .fault_factory(move || Box::new(ProbeInjector(factory_probe.clone())))
        .chaos_duration(CHAOS)
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "seed {seed}: {report}");
    let boots = lock(&boots).clone();
    let probe = lock(&probe).clone();
    (boots, probe)
}

fn boots_of<'b>(boots: &'b [(String, Duration)], ip: &str) -> Vec<&'b Duration> {
    boots
        .iter()
        .filter(|(at, _)| at == ip)
        .map(|(_, t)| t)
        .collect()
}

#[test]
fn a_group_outage_downs_every_member_at_once_and_brings_each_back() {
    for seed in [1, 2, 3, 42, 1234] {
        let (boots, probe) = run_seed(seed);
        let landed = probe.landed.clone().expect("the outage landed");

        // Every member of the group, at one instant; the other untouched.
        assert_eq!(landed.victims, ["10.0.1.1", "10.0.1.2", "10.0.1.3"]);
        assert_eq!(probe.all_down_at_once, Some(true), "seed {seed}");
        for ip in ["10.0.2.1", "10.0.2.2"] {
            assert_eq!(boots_of(&boots, ip).len(), 1, "seed {seed}: {ip} rebooted");
        }

        // Each victim back after its own drawn delay.
        let window = Duration::from_millis(1);
        let mut returns = Vec::new();
        for (ip, down) in landed.victims.iter().zip(&landed.down) {
            let booted = boots_of(&boots, ip);
            assert_eq!(booted.len(), 2, "seed {seed}: {ip} boots {booted:?}");
            let back = *booted[1];
            let expected = landed.at + *down;
            assert!(
                back >= expected && back <= expected + window,
                "seed {seed}: {ip} back at {back:?}, expected {expected:?}"
            );
            returns.push((back, ip.clone()));
        }

        // The straggler last, after the chaos window closed.
        let straggler = landed.straggler.clone().expect("a straggler was drawn");
        returns.sort();
        let (last_back, last) = returns.last().expect("victims returned");
        assert_eq!(*last, straggler, "seed {seed}: {returns:?}");
        assert!(returns[returns.len() - 2].0 < *last_back);
        assert!(
            *last_back > CHAOS,
            "seed {seed}: the straggler returned in the window"
        );

        // Nobody is left down.
        let recovered = probe.recovered_at.expect("the dead set drained");
        assert!(recovered >= *last_back, "seed {seed}");
    }
}

#[test]
fn the_same_seed_replays_the_same_kill_and_restart_schedule() {
    let first = run_seed(7);
    let second = run_seed(7);
    assert_eq!(first, second);
    let other = run_seed(8);
    assert_ne!(
        first.1.landed, other.1.landed,
        "two seeds drew one schedule"
    );
}

#[test]
fn a_swarmed_outage_strikes_on_some_seeds_only() {
    let mut struck = Vec::new();
    for seed in 1..=12 {
        let probe: Shared<Probe> = Arc::default();
        let factory_probe = probe.clone();
        let boots: Boots = Arc::default();
        let report = SimulationBuilder::new()
            .processes(2, node("acceptor", &boots))
            .processes(1, node("matchmaker", &boots))
            .workload(Patience)
            .enable_chaos([Chaos::Outage {
                config: outage(),
                mode: ChaosMode::Swarm,
            }])
            .fault_factory(move || Box::new(ProbeInjector(factory_probe.clone())))
            .chaos_duration(CHAOS)
            .set_iterations(1)
            .set_debug_seeds(vec![seed])
            .run()
            .expect("simulation configuration is valid");
        assert_eq!(report.failed_runs, 0, "seed {seed}: {report}");
        struck.push(lock(&probe).landed.is_some());
    }
    assert!(struck.contains(&true), "{struck:?}");
    assert!(struck.contains(&false), "{struck:?}");
}

#[test]
fn an_outage_over_an_unregistered_group_is_refused() {
    let boots: Boots = Arc::default();
    let result = SimulationBuilder::new()
        .processes(1, node("acceptor", &boots))
        .workload(Patience)
        .enable_chaos([Chaos::Outage {
            config: Outage::groups(["learner"]),
            mode: ChaosMode::Random,
        }])
        .chaos_duration(CHAOS)
        .run();
    assert!(
        matches!(result, Err(SimulationError::InvalidConfiguration(ref m)) if m.contains("learner")),
        "{result:?}"
    );
}

#[test]
fn a_straggler_range_inside_down_is_refused() {
    let boots: Boots = Arc::default();
    let result = SimulationBuilder::new()
        .processes(1, node("acceptor", &boots))
        .workload(Patience)
        .enable_chaos([Chaos::Outage {
            config: Outage {
                straggler: Some(Duration::from_secs(1)..Duration::from_secs(3)),
                ..Outage::groups(["acceptor"])
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(CHAOS)
        .run();
    assert!(
        matches!(result, Err(SimulationError::InvalidConfiguration(ref m)) if m.contains("straggler")),
        "{result:?}"
    );
}
