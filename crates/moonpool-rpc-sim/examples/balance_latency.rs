//! Tail latency of one typed call over localhost TCP at a fixed offered
//! load: a single target, the same three sessions called round robin
//! without a balancer, a balanced client without copies, and a hedged
//! balanced client.
//!
//! Three servers answer at once, except that each request stalls with a
//! small probability (a GC pause, a slow disk). Calls arrive open-loop at
//! a fixed rate, so every configuration sees the same offered load; the
//! report gives latency percentiles and how many handler executions each
//! call cost (hedges execute some requests twice). A calibration point,
//! not a benchmark suite: numbers depend on the machine.
//!
//! ```text
//! cargo run --release -p moonpool-rpc-sim --example balance_latency \
//!     [calls_per_second=1000] [seconds=5] [stall_ms=20] [stall_percent=5]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moonpool_core::{Providers, RandomProvider, TimeProvider, TokioProviders};
use moonpool_rpc::balance::{
    Alternative, AlternativeSet, BalanceConfig, BalancePolicy, BalancedClient, HedgeBudget,
    ModelConfig, QueueModel, SetVersion,
};
use moonpool_rpc::{
    AccessClass, IncomingRequest, RpcConfig, RpcDriver, RpcHandle, ServiceClient, ServiceRef,
};
use moonpool_rpc_sim::foundations::messages::{Echo, Echoed, Probe};

/// How servers misbehave.
#[derive(Clone, Copy)]
struct Stall {
    duration: Duration,
    probability: f64,
}

/// One server with occasional stalls; counts its handler executions.
async fn server(
    stall: Stall,
    executions: Arc<AtomicU64>,
) -> Result<ServiceRef<Echo>, Box<dyn std::error::Error>> {
    let providers = TokioProviders::new();
    let (driver, rpc) =
        RpcDriver::listen(providers.clone(), "127.0.0.1:0", RpcConfig::default()).await?;
    tokio::spawn(driver.run());
    let (service, mut stream) = rpc.register::<Echo>(AccessClass::Public)?;
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            executions.fetch_add(1, Ordering::Relaxed);
            let stalls = providers.random().random_bool(stall.probability);
            let time = providers.time().clone();
            tokio::spawn(async move {
                if stalls {
                    let _ = time.sleep(stall.duration).await;
                }
                let _ = reply.send(&Echoed {
                    id: request.id,
                    text: request.text,
                });
            });
        }
    });
    Ok(service)
}

/// How one configuration calls.
enum Caller {
    Single(ServiceClient<TokioProviders, Echo>),
    RoundRobin(Vec<ServiceClient<TokioProviders, Echo>>),
    Balanced(BalancedClient<TokioProviders, Echo>, BalancePolicy),
}

impl Caller {
    async fn call(&self, id: u64) -> bool {
        match self {
            Self::Single(client) => client.try_get_reply(&Probe::new(id)).await.is_ok(),
            Self::RoundRobin(clients) => {
                let index = usize::try_from(id).unwrap_or(0) % clients.len().max(1);
                match clients.get(index) {
                    Some(client) => client.try_get_reply(&Probe::new(id)).await.is_ok(),
                    None => false,
                }
            }
            Self::Balanced(client, policy) => client.call(Probe::new(id), policy).await.is_ok(),
        }
    }
}

struct Report {
    name: &'static str,
    samples: Vec<Duration>,
    failed: u64,
    executions: u64,
    calls: u64,
}

impl Report {
    fn print(&mut self) {
        self.samples.sort();
        let at = |per_mille: usize| {
            self.samples
                .get(self.samples.len().saturating_sub(1) * per_mille / 1000)
                .copied()
                .unwrap_or_default()
        };
        let as_f64 = |count: u64| f64::from(u32::try_from(count).unwrap_or(u32::MAX));
        let cost = as_f64(self.executions) / as_f64(self.calls.max(1));
        println!(
            "| {:<18} | {:>6} | {:>9.2?} | {:>9.2?} | {:>9.2?} | {:>9.2?} | {:>6} | {:>6.3} |",
            self.name,
            self.calls,
            at(500),
            at(990),
            at(999),
            at(1000),
            self.failed,
            cost
        );
    }
}

/// Offer `rate` calls per second for `seconds`, open-loop.
async fn run(
    name: &'static str,
    caller: Arc<Caller>,
    executions: &[Arc<AtomicU64>],
    load: (u32, u32),
    settle: Duration,
) -> Report {
    let (rate, seconds) = load;
    let time = TokioProviders::new();
    let before: u64 = executions
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    let calls = u64::from(rate) * u64::from(seconds);
    let interval = Duration::from_secs(1) / rate.max(1);
    let started = Instant::now();
    let failed = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(usize::try_from(calls).unwrap_or(0));
    for id in 0..calls {
        let due = started + interval * u32::try_from(id).unwrap_or(u32::MAX);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            let _ = time.time().sleep(wait).await;
        }
        let caller = Arc::clone(&caller);
        let failed = Arc::clone(&failed);
        handles.push(tokio::spawn(async move {
            let call = Instant::now();
            if !caller.call(id).await {
                failed.fetch_add(1, Ordering::Relaxed);
            }
            call.elapsed()
        }));
    }
    let mut samples = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(sample) = handle.await {
            samples.push(sample);
        }
    }
    // Let late losers land before counting executions.
    let _ = time.time().sleep(settle).await;
    let after: u64 = executions
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    Report {
        name,
        samples,
        failed: failed.load(Ordering::Relaxed),
        executions: after - before,
        calls,
    }
}

fn balanced(
    rpc: &RpcHandle<TokioProviders>,
    services: &[ServiceRef<Echo>],
) -> Result<(BalancedClient<TokioProviders, Echo>, QueueModel), Box<dyn std::error::Error>> {
    let model = QueueModel::new(ModelConfig {
        // Start with FoundationDB's full budget: the comparison is about
        // steady-state tails, not budget warm-up.
        hedge_budget: HedgeBudget {
            initial: 100.0,
            ..HedgeBudget::default()
        },
        ..ModelConfig::default()
    })?;
    let set = AlternativeSet::new(
        SetVersion::new(1),
        services
            .iter()
            .cloned()
            .map(Alternative::anywhere)
            .collect(),
    )?;
    let client = BalancedClient::new(rpc, set, model.clone(), BalanceConfig::default())?;
    Ok((client, model))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut next = |default: u32| {
        args.next()
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(default)
    };
    let rate = next(1000);
    let seconds = next(5);
    let stall = Stall {
        duration: Duration::from_millis(u64::from(next(20))),
        probability: f64::from(next(5)) / 100.0,
    };
    let executions: Vec<_> = (0..3).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut services = Vec::new();
    for count in &executions {
        services.push(server(stall, Arc::clone(count)).await?);
    }
    let (client_driver, rpc) = RpcDriver::client_only(TokioProviders::new(), RpcConfig::default())?;
    tokio::spawn(client_driver.run());

    let (unhedged, _) = balanced(&rpc, &services)?;
    let (hedged, hedged_model) = balanced(&rpc, &services)?;
    tokio::spawn(hedged_model.collect_lagging());
    // Warm every session up outside the measurement.
    for service in &services {
        service.bind(&rpc).try_get_reply(&Probe::new(0)).await?;
    }

    println!(
        "{rate} calls/s for {seconds} s, 3 servers, {:.0}% of requests stall {:?}\n",
        stall.probability * 100.0,
        stall.duration
    );
    println!(
        "| configuration      |  calls |       p50 |       p99 |     p99.9 |       max | failed | executions/call |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    let configurations = [
        ("single target", Caller::Single(services[0].bind(&rpc))),
        // The same three sessions without a balancer: separates the
        // balancer's cost from the transport's with several connections.
        (
            "plain round robin",
            Caller::RoundRobin(services.iter().map(|service| service.bind(&rpc)).collect()),
        ),
        (
            "balanced",
            Caller::Balanced(unhedged, BalancePolicy::default()),
        ),
        (
            "balanced + hedged",
            Caller::Balanced(hedged, BalancePolicy::hedged()),
        ),
    ];
    for (name, caller) in configurations {
        run(
            name,
            Arc::new(caller),
            &executions,
            (rate, seconds),
            stall.duration * 2,
        )
        .await
        .print();
    }
    let stats = hedged_model.stats();
    println!(
        "\nhedges granted {}, denied for budget {}, late losers completed {}",
        stats.copies_granted, stats.copies_denied, stats.lagging_completed
    );
    Ok(())
}
