//! Workloads change the deployment mid-run through `ctx.deployment()`.
//!
//! Covers `spawn` (the next IP of the group, booted from its factory,
//! reachable over the network), `spawn_group` (the next `/24`, ranks
//! `0..n`), `retire` (never restarted, out of the topology, disk still
//! readable), spawns under attrition (a victim only after it boots, and it
//! restarts), the `max_spawns` cap, a process's refused handle, and replay:
//! the same seed spawns at the same time with the same IP.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, FaultContext, FaultInjector,
    NetworkProvider, Process, SimContext, SimulationBuilder, SimulationError, SimulationResult,
    TcpListenerTrait, TimeProvider, Workload,
};

/// `(sim time in ms, ip)` of every boot, shared with the factories.
type BootLog = Arc<Mutex<Vec<(u64, String)>>>;

fn boot_log() -> BootLog {
    Arc::new(Mutex::new(Vec::new()))
}

fn boots(log: &BootLog) -> Vec<(u64, String)> {
    log.lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

fn now_ms(ctx: &SimContext) -> u64 {
    u64::try_from(ctx.time().now().as_millis()).unwrap_or(u64::MAX)
}

/// Records its boot, writes and syncs a `data` file, then answers every
/// connection with one byte until shutdown. Its deployment handle refuses.
struct Server {
    boots: BootLog,
}

impl Server {
    fn factory(boots: &BootLog) -> impl Fn() -> Box<dyn Process> + Send + Sync + 'static {
        let boots = boots.clone();
        move || {
            Box::new(Server {
                boots: boots.clone(),
            })
        }
    }
}

#[async_trait]
impl Process for Server {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.boots
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((now_ms(ctx), ctx.my_ip().to_string()));
        assert!(
            ctx.deployment().spawn("server").is_err(),
            "a process cannot change the deployment"
        );
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        if let Ok(file) = ctx
            .storage()
            .open("data", OpenOptions::create_write().read(true))
            .await
        {
            let _ = file.write_at(0, ctx.my_ip().as_bytes()).await;
            let _ = file.sync_all().await;
        }
        loop {
            let accepted = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            if let Ok((mut stream, _)) = accepted {
                let _ = stream.write_all(&[7]).await;
            }
        }
    }
}

fn invalid(message: impl Into<String>) -> SimulationError {
    SimulationError::InvalidState(message.into())
}

/// Connect to `ip` until it answers its byte, every attempt bounded so a
/// connect before the listener binds can refuse or connect, never hang.
async fn reach(ctx: &SimContext, ip: &str) -> SimulationResult<()> {
    for _ in 0..50 {
        let attempt = async {
            let mut stream = ctx.network().connect(ip).await?;
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).await?;
            Ok::<_, std::io::Error>(byte[0])
        };
        match ctx.time().timeout(Duration::from_secs(5), attempt).await {
            Ok(Ok(7)) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err(invalid(format!("connect to {ip} hung"))),
        }
        let _ = ctx.time().sleep(Duration::from_millis(50)).await;
    }
    Err(invalid(format!("{ip} never answered")))
}

// ============================================================================
// spawn, then connect
// ============================================================================

struct SpawnAndConnect;

#[async_trait]
impl Workload for SpawnAndConnect {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let before = ctx.deployment().version();
        let ip = ctx.deployment().spawn("server")?;
        if ip != "10.0.1.3" {
            return Err(invalid(format!("spawn took {ip}, not the next host")));
        }
        // Connect at once: the process is not even booted yet.
        reach(ctx, &ip).await?;
        let topo = ctx.topology();
        if topo.ips_in_group("server") != ["10.0.1.1", "10.0.1.2", "10.0.1.3"] {
            return Err(invalid(format!("{:?}", topo.ips_in_group("server"))));
        }
        if ctx.deployment().version() != before + 1 || topo.membership_version() != before + 1 {
            return Err(invalid("version did not move on spawn"));
        }
        if !topo.all_process_ips().contains(&ip) {
            return Err(invalid("all_process_ips misses the spawned process"));
        }
        Ok(())
    }
}

#[test]
fn a_spawned_process_boots_on_the_next_ip_and_accepts() {
    let log = boot_log();
    let report = SimulationBuilder::new()
        .processes(2, Server::factory(&log))
        .workload(SpawnAndConnect)
        .set_iterations(4)
        .set_debug_seeds(vec![1, 2, 3, 4])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    for metrics in report.individual_metrics.iter().flatten() {
        assert_eq!(
            metrics.deployment.spawned.get("server"),
            Some(&vec!["10.0.1.3".to_string()])
        );
    }
    let spawned_boots = boots(&log)
        .into_iter()
        .filter(|(_, ip)| ip == "10.0.1.3")
        .count();
    assert_eq!(spawned_boots, 4, "one boot of the spawned process per seed");
}

// ============================================================================
// spawn_group
// ============================================================================

/// Records `(ip, client_id, client_count)` once its group is complete.
struct Spare {
    ranks: Arc<Mutex<Vec<(String, usize, usize)>>>,
}

#[async_trait]
impl Process for Spare {
    fn name(&self) -> &'static str {
        "spare"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;
        self.ranks
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((ctx.my_ip().to_string(), ctx.client_id(), ctx.client_count()));
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

struct StartGroup {
    ranks: Arc<Mutex<Vec<(String, usize, usize)>>>,
}

#[async_trait]
impl Workload for StartGroup {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ranks = self.ranks.clone();
        let ips = ctx.deployment().spawn_group("spare", 3, move || {
            Box::new(Spare {
                ranks: ranks.clone(),
            })
        })?;
        if ips != ["10.0.2.1", "10.0.2.2", "10.0.2.3"] {
            return Err(invalid(format!("spawn_group took {ips:?}")));
        }
        if ctx
            .deployment()
            .spawn_group("spare", 1, || {
                Box::new(Spare {
                    ranks: Arc::default(),
                })
            })
            .is_ok()
        {
            return Err(invalid("a group name is taken once"));
        }
        let _ = ctx.time().sleep(Duration::from_millis(500)).await;
        if ctx.topology().groups() != ["server", "spare"] {
            return Err(invalid(format!("{:?}", ctx.topology().groups())));
        }
        // A later spawn adds to the new group.
        if ctx.deployment().spawn("spare")? != "10.0.2.4" {
            return Err(invalid("spawn into a started group"));
        }
        Ok(())
    }
}

#[test]
fn spawn_group_takes_the_next_range_and_ranks_from_zero() {
    let log = boot_log();
    let ranks = Arc::new(Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .processes(1, Server::factory(&log))
        .workload(StartGroup {
            ranks: ranks.clone(),
        })
        .set_iterations(1)
        .set_debug_seeds(vec![5])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let mut ranks = ranks
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone();
    ranks.sort();
    assert_eq!(
        ranks[..3],
        [
            ("10.0.2.1".to_string(), 0, 3),
            ("10.0.2.2".to_string(), 1, 3),
            ("10.0.2.3".to_string(), 2, 3),
        ]
    );
}

// ============================================================================
// retire
// ============================================================================

const RETIRED_KEY: &str = "test:retired";
const DISK_KEY: &str = "test:retired-disk";

struct Retire;

#[async_trait]
impl Workload for Retire {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_secs(1)).await;
        ctx.deployment().retire("10.0.1.2")?;
        ctx.state().publish(RETIRED_KEY, "10.0.1.2".to_string());
        if ctx.topology().ips_in_group("server") != ["10.0.1.1", "10.0.1.3"] {
            return Err(invalid("a retired process leaves the group at once"));
        }
        if ctx.deployment().retire("10.0.1.2").is_ok() {
            return Err(invalid("a process retires once"));
        }
        // The next spawn never reuses the retired IP.
        if ctx.deployment().spawn("server")? != "10.0.1.4" {
            return Err(invalid("a retired IP was reused"));
        }
        let _ = ctx.time().sleep(Duration::from_secs(20)).await;
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        match ctx.state().get::<Vec<u8>>(DISK_KEY) {
            Some(bytes) if bytes.starts_with(b"10.0.1.2") => Ok(()),
            other => Err(invalid(format!("retired disk unreadable: {other:?}"))),
        }
    }
}

/// Reads the retired process's disk after the retire and publishes it, and
/// tries to restart it (refused).
struct DiskReader;

#[async_trait]
impl FaultInjector for DiskReader {
    fn name(&self) -> &'static str {
        "disk_reader"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        loop {
            let _ = ctx.time().sleep(Duration::from_millis(500)).await;
            if let Some(ip) = ctx.state().get::<String>(RETIRED_KEY) {
                let _ = ctx.time().sleep(Duration::from_secs(1)).await;
                ctx.restart(&ip)?;
                ctx.reboot(&ip, moonpool_sim::RebootKind::Crash)?;
                let bytes = ctx.durable_bytes(&ip, "data")?;
                ctx.state().publish(DISK_KEY, bytes);
                return Ok(());
            }
        }
    }
}

#[test]
fn a_retired_process_never_restarts_and_keeps_its_disk() {
    let log = boot_log();
    let report = SimulationBuilder::new()
        .processes(3, Server::factory(&log))
        .workload(Retire)
        .fault_factory(|| Box::new(DiskReader))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.0,
                prob_crash: 1.0,
                prob_wipe: 0.0,
                recovery_delay_ms: Some(100..500),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(15))
        .set_iterations(4)
        .set_debug_seeds(vec![11, 12, 13, 14])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    // .2 boots only before its retire at 1s.
    for (at, ip) in boots(&log) {
        if ip == "10.0.1.2" {
            assert!(at < 1000, "the retired process booted again at {at}ms");
        }
    }
    for metrics in report.individual_metrics.iter().flatten() {
        assert_eq!(
            metrics.deployment.retired.get("server"),
            Some(&vec!["10.0.1.2".to_string()])
        );
    }
}

// ============================================================================
// spawn under attrition
// ============================================================================

const SPAWNS_KEY: &str = "test:spawns";

/// Spawns two servers during the chaos window, recording when.
struct SpawnUnderChaos;

#[async_trait]
impl Workload for SpawnUnderChaos {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut spawns: Vec<(u64, String)> = Vec::new();
        for _ in 0..2 {
            let pause = moonpool_sim::sim_random_range(500..3000u64);
            let _ = ctx.time().sleep(Duration::from_millis(pause)).await;
            let ip = ctx.deployment().spawn("server")?;
            spawns.push((now_ms(ctx), ip));
            ctx.state().publish(SPAWNS_KEY, spawns.clone());
        }
        let _ = ctx.time().sleep(Duration::from_secs(30)).await;
        Ok(())
    }
}

fn attrition() -> Chaos {
    Chaos::Attrition {
        config: Attrition {
            max_dead: 1,
            prob_graceful: 0.3,
            prob_crash: 0.7,
            prob_wipe: 0.0,
            recovery_delay_ms: Some(100..1000),
            grace_period_ms: Some(100..500),
            scope: AttritionScope::PerProcess,
            victims: AttritionVictims::Any,
        },
        mode: ChaosMode::Random,
    }
}

#[test]
fn a_spawned_process_is_a_victim_once_booted_and_restarts() {
    let log = boot_log();
    let seeds: Vec<u64> = (20..36).collect();
    let report = SimulationBuilder::new()
        .processes(2, Server::factory(&log))
        .workload(SpawnUnderChaos)
        .enable_chaos([attrition()])
        .chaos_duration(Duration::from_secs(25))
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    let restarted = report
        .assertion_details
        .iter()
        .find(|detail| detail.msg == "deployment: a process spawned mid-run restarts")
        .map_or(0, |detail| detail.pass_count);
    assert!(restarted > 0, "no spawned process was killed and restarted");
}

// ============================================================================
// max_spawns, and a workload-free group start
// ============================================================================

struct OverSpawn;

#[async_trait]
impl Workload for OverSpawn {
    fn name(&self) -> &'static str {
        "operator"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.deployment().spawn("server")?;
        ctx.deployment().spawn("server")?;
        if ctx.deployment().spawn("server").is_ok() {
            return Err(invalid("the cap allowed a third spawn"));
        }
        if ctx.deployment().spawn("nobody").is_ok() {
            return Err(invalid("spawn into an unknown group"));
        }
        Ok(())
    }
}

#[test]
fn max_spawns_caps_the_run() {
    let log = boot_log();
    let report = SimulationBuilder::new()
        .processes(1, Server::factory(&log))
        .workload(OverSpawn)
        .max_spawns(2)
        .set_iterations(1)
        .set_debug_seeds(vec![3])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
}

// ============================================================================
// replay
// ============================================================================

#[test]
fn the_same_seed_spawns_at_the_same_time_with_the_same_ip() {
    let run = || {
        let log = boot_log();
        let report = SimulationBuilder::new()
            .processes(2, Server::factory(&log))
            .workload_factory(|| Box::new(SpawnUnderChaos))
            .enable_chaos([attrition()])
            .chaos_duration(Duration::from_secs(25))
            .check_determinism()
            .set_iterations(1)
            .set_debug_seeds(vec![42])
            .run()
            .expect("simulation configuration is valid");
        assert_eq!(report.failed_runs, 0, "{report}");
        assert!(report.assertion_violations.is_empty(), "{report}");
        boots(&log)
    };
    let first = run();
    assert!(first.iter().any(|(_, ip)| ip == "10.0.1.3"));
    assert_eq!(first, run());
}
