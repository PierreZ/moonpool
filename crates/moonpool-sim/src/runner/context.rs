//! The simulation context handed to processes and workloads.
//!
//! [`SimContext`] is the single entry point from inside the simulation to
//! its infrastructure: the providers, the topology, the shared state, the
//! shutdown signal, the observability timeline, per-node metrics, and a
//! process's own crash.
//!
//! # Usage
//!
//! ```rust
//! use moonpool_sim::{NetworkProvider, SimContext, SimulationResult};
//!
//! async fn client(ctx: &SimContext) -> SimulationResult<()> {
//!     // Process IPs come from the topology; groups are named after their
//!     // process type.
//!     let Some(server) = ctx.topology().ips_in_group("server").first().cloned() else {
//!         return Ok(());
//!     };
//!     let _stream = ctx.network().connect(&server).await?;
//!     // Tell the other actors of this run what happened.
//!     ctx.state().publish("connected", true);
//!     Ok(())
//! }
//! ```

use std::sync::Arc;

use moonpool_core::metrics::MetricsSource;

use crate::chaos::state_handle::StateHandle;
use crate::network::SimNetworkProvider;
use crate::observability::SimulationLayerHandle;
use crate::providers::{SimProviders, SimRandomProvider, SimTimeProvider};
use crate::storage::SimStorageProvider;

use moonpool_core::Providers;

use super::app_metrics::MetricsHandle;
use super::deployment::Deployment;
use super::topology::WorkloadTopology;

/// Simulation context provided to processes and workloads.
///
/// Wraps all simulation infrastructure into a single, non-generic struct.
/// For code generic over `P: Providers`, pass `ctx.providers()`: that is how
/// shipped code, written against the provider traits, runs in simulation.
///
/// A process gets a fresh context on every boot, with fresh providers. A
/// workload keeps one context for the whole iteration. A clone shares every
/// handle, so a workload can hand its context to a task it spawns (a client
/// program that runs on from `run()` into `check()` under
/// [`chaos_until_run_ends`](crate::SimulationBuilder::chaos_until_run_ends)).
#[derive(Clone)]
pub struct SimContext {
    providers: SimProviders,
    topology: WorkloadTopology,
    state: StateHandle,
    obs: SimulationLayerHandle,
    metrics: MetricsHandle,
    deployment: Deployment,
}

impl SimContext {
    /// Create a new simulation context.
    ///
    /// The runner builds one per process boot and per workload; call this
    /// only to drive a [`SimWorld`](crate::SimWorld) by hand.
    #[must_use]
    pub fn new(
        providers: SimProviders,
        topology: WorkloadTopology,
        state: StateHandle,
        obs: SimulationLayerHandle,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            providers,
            topology,
            state,
            obs,
            metrics,
            deployment: Deployment::detached(),
        }
    }

    /// Attach the run's deployment handle.
    pub(crate) fn with_deployment(mut self, deployment: Deployment) -> Self {
        self.deployment = deployment;
        self
    }

    /// The handle a workload changes the deployment with: spawn a process,
    /// start a group, retire a process (see [`Deployment`]).
    ///
    /// A process gets a handle too, but its calls refuse: only a client
    /// changes the deployment. A hand-built context's handle is detached and
    /// refuses as well.
    #[must_use]
    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    /// Get the full providers bundle for passing to generic code.
    ///
    /// Code that takes `P: Providers` (the same type parameter production
    /// fills with `TokioProviders`) gets this bundle in simulation.
    #[must_use]
    pub fn providers(&self) -> &SimProviders {
        &self.providers
    }

    /// Get the simulated network provider, bound to this actor's IP.
    ///
    /// Bind with [`my_ip`](Self::my_ip) as the address; connect to an IP
    /// from the [`topology`](Self::topology).
    #[must_use]
    pub fn network(&self) -> &SimNetworkProvider {
        self.providers.network()
    }

    /// Get the simulated time provider: logical time that moves only when
    /// the world steps.
    ///
    /// Use it for every sleep, timeout and timestamp. Durations measured
    /// with it replay identically; wall-clock time does not.
    #[must_use]
    pub fn time(&self) -> &SimTimeProvider {
        self.providers.time()
    }

    /// Get the task provider, which spawns on the deterministic executor.
    ///
    /// Tasks a process spawns belong to that process: a reboot aborts them,
    /// and the CPU gate charges their polls to its core.
    #[must_use]
    pub fn task(&self) -> &crate::providers::SimTaskProvider {
        self.providers.task()
    }

    /// Run `body`, a future that need not be `Send`, as a local task of
    /// this process or workload, and return its answer.
    ///
    /// [`Process::run`](crate::Process::run) and
    /// [`Workload::run`](crate::Workload::run) return `Send` futures. Code
    /// written for a thread-per-core runtime (the local provider traits)
    /// holds `!Send` values across its awaits, so it cannot run inside them
    /// directly. This runs it on the same deterministic executor and waits
    /// for its answer over a channel. `body` gets its own clone of the
    /// context, because a task outlives any borrow.
    ///
    /// The task belongs to the caller's boot scope: a crash or a reboot
    /// ends it with the process. When the task ends without an answer (it
    /// was aborted), the returned future never completes: the runner is
    /// about to drop it.
    ///
    /// ```ignore
    /// async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    ///     ctx.run_local("node", |ctx| async move { local_node(&ctx).await }).await
    /// }
    /// ```
    pub async fn run_local<T, F>(&self, name: &str, body: impl FnOnce(SimContext) -> F) -> T
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + 'static,
    {
        use moonpool_core::LocalTaskProvider;

        let (answer, answered) = futures::channel::oneshot::channel();
        let future = body(self.clone());
        LocalTaskProvider::spawn_local_task(self.task(), name, async move {
            // The caller is gone only when its process is: nothing to tell.
            let _ = answer.send(future.await);
        })
        .detach();
        match answered.await {
            Ok(answer) => answer,
            Err(_) => std::future::pending().await,
        }
    }

    /// Get the seeded random provider.
    ///
    /// Every draw comes from the simulation's one random stream, so it is
    /// part of the seed's replay. Never use an OS random source in
    /// simulated code.
    #[must_use]
    pub fn random(&self) -> &SimRandomProvider {
        self.providers.random()
    }

    /// Get the simulated storage provider, scoped to this actor's IP.
    ///
    /// Files opened through it belong to this process: its crashes resolve
    /// their unsynced writes, a wipe deletes them, and storage chaos aims at
    /// them. Files survive a process reboot; in-memory state does not.
    #[must_use]
    pub fn storage(&self) -> &SimStorageProvider {
        self.providers.storage()
    }

    /// The cumulative counters of the simulated disk of the process at `ip`
    /// now, the simulator's ground truth (see
    /// [`SimWorld::disk_counters`](crate::SimWorld::disk_counters)). `None`
    /// once the simulation is gone.
    #[must_use]
    pub fn disk_counters(&self, ip: std::net::IpAddr) -> Option<moonpool_core::DiskCounters> {
        let world = self.providers.weak_world().upgrade().ok()?;
        Some(world.disk_counters(ip))
    }

    /// The last system samples the process at `ip` was handed, oldest first
    /// (see [`SimWorld::system_samples`](crate::SimWorld::system_samples)).
    #[must_use]
    pub fn system_samples(&self, ip: std::net::IpAddr) -> Vec<moonpool_core::SystemSample> {
        self.providers
            .weak_world()
            .upgrade()
            .map(|world| world.system_samples(ip))
            .unwrap_or_default()
    }

    /// How slow the process at `ip` is now: its gray-failure factors (see
    /// [`SimWorld::slowness`](crate::SimWorld::slowness)). Healthy once the
    /// simulation is gone.
    #[must_use]
    pub fn slowness(&self, ip: std::net::IpAddr) -> crate::Slowness {
        self.providers
            .weak_world()
            .upgrade()
            .map_or(crate::Slowness::HEALTHY, |world| world.slowness(ip))
    }

    /// Get this actor's IP address: `10.0.{group}.{n}` for a process,
    /// `10.0.0.{n}` for a workload.
    #[must_use]
    pub fn my_ip(&self) -> &str {
        &self.topology.my_ip
    }

    /// Get this actor's client ID.
    ///
    /// For a workload: sequential from 0 among the instances of its builder
    /// entry (FDB-style); a single `.workload(instance)` is always client 0.
    /// For a process: its rank within its own process group now, read live
    /// (a spawned process takes the next rank; a retire moves the ranks
    /// above it down).
    #[must_use]
    pub fn client_id(&self) -> usize {
        self.topology.client_id()
    }

    /// Get the total number of instances sharing this actor's entry.
    ///
    /// For single `.workload()` entries this is 1.
    /// For `.workloads(count, factory)` entries this is the resolved count.
    /// For a process, it is the size of its process group now.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.topology.client_count()
    }

    /// Find a peer workload's IP address by its [`Workload::name`](crate::Workload::name).
    ///
    /// For process IPs, use [`topology`](Self::topology) and
    /// [`WorkloadTopology::ips_in_group`].
    #[must_use]
    pub fn peer(&self, name: &str) -> Option<String> {
        self.topology.peer_by_name(name)
    }

    /// Get every peer workload as `(name, ip)` pairs.
    #[must_use]
    pub fn peers(&self) -> Vec<(String, String)> {
        self.topology
            .peer_names
            .iter()
            .zip(self.topology.peer_ips.iter())
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }

    /// Get the shutdown cancellation token.
    ///
    /// It fires for everyone once the first workload returns from
    /// [`Workload::run`](crate::Workload::run), or when a stall guard trips.
    /// A process's token also fires at the start of a graceful reboot, and
    /// the process then has a grace period to return. Long-running loops should
    /// `select!` on `shutdown().cancelled()`.
    #[must_use]
    pub fn shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.topology.shutdown_signal
    }

    /// Crash this process now, as a power loss would, from inside it: see
    /// [`SelfCrash::crash`], which this is a shorthand for.
    ///
    /// # Errors
    ///
    /// As [`self_crash`](Self::self_crash) and [`SelfCrash::crash`].
    pub fn crash_self(
        &self,
        kind: crate::RebootKind,
        recovery_delay: Option<std::time::Duration>,
    ) -> crate::SimulationResult<()> {
        self.self_crash()?.crash(kind, recovery_delay)
    }

    /// A handle this process can crash itself with, from code that holds no
    /// [`SimContext`]: it is `Clone + Send + Sync + 'static`, so it can ride
    /// into a driver's hooks or a spawned task.
    ///
    /// # Errors
    ///
    /// [`SimulationError::InvalidState`](crate::SimulationError::InvalidState)
    /// from a workload: only a process can crash itself.
    pub fn self_crash(&self) -> crate::SimulationResult<SelfCrash> {
        let ip: std::net::IpAddr = self.my_ip().parse().map_err(|error| {
            crate::SimulationError::InvalidState(format!("invalid IP '{}': {error}", self.my_ip()))
        })?;
        if self.topology.membership.group_for(ip).is_none() {
            return Err(crate::SimulationError::InvalidState(
                "crash_self: only a process can crash itself, not a workload".to_string(),
            ));
        }
        Ok(SelfCrash {
            sim: self.providers.weak_world(),
            ip,
        })
    }

    /// Get the topology this actor sees: process IPs by group, tag and
    /// failure domain, peer workloads, and its own tags and locality.
    #[must_use]
    pub fn topology(&self) -> &WorkloadTopology {
        &self.topology
    }

    /// Get the shared state handle for this run.
    ///
    /// Processes, workloads, fault injectors and the harness's vetoes
    /// ([`HintVeto`](crate::HintVeto), [`OutageVeto`](crate::OutageVeto))
    /// share it. It is fresh for each iteration and survives process
    /// reboots.
    #[must_use]
    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// Get a clonable handle to the observability layer.
    ///
    /// The handle implements [`crate::TraceQuery`], so workloads can read
    /// the captured timeline (e.g. in `check()`) the same way invariants do.
    /// To get events INTO the timeline, emit plain `tracing` events — e.g.
    /// `tracing::info!(term, leader = %ip, "leader_elected")` — from inside
    /// a process or workload task; the orchestrator's actor spans attribute
    /// them automatically.
    #[must_use]
    pub fn observability(&self) -> &SimulationLayerHandle {
        &self.obs
    }

    /// Get this node's application metrics source, downcast to its type.
    ///
    /// The source is the one
    /// [`metrics_factory`](super::builder::SimulationBuilder::metrics_factory)
    /// built for [`my_ip`](Self::my_ip), so each simulated node counts
    /// independently. Returns `None` when no factory was configured, or when
    /// `S` is not the type the factory produced.
    ///
    /// ```ignore
    /// let metrics = ctx.metrics::<PrometheusSource>().expect("metrics configured");
    /// metrics.int_counter("requests_total", "Requests served")?.inc();
    /// ```
    ///
    /// The source outlives process reboots (it is keyed by IP, not by process
    /// instance), so a metric registered on first boot is still registered
    /// after a restart — look counters up rather than re-registering them.
    #[must_use]
    pub fn metrics<S: MetricsSource>(&self) -> Option<Arc<S>> {
        self.metrics.get(self.my_ip())
    }

    /// Get the raw handle to every node's metrics sources.
    ///
    /// Workloads use this to read a *server's* counters in `check()`;
    /// [`metrics`](Self::metrics) is the usual per-node accessor.
    #[must_use]
    pub fn metrics_handle(&self) -> &MetricsHandle {
        &self.metrics
    }
}

/// A process's handle on its own crash ([`SimContext::self_crash`]).
#[derive(Debug, Clone)]
pub struct SelfCrash {
    sim: crate::sim::WeakSimWorld,
    ip: std::net::IpAddr,
}

impl SelfCrash {
    /// Crash this process now, as a power loss would.
    ///
    /// Schedules the same force kill a fault injector's
    /// [`reboot`](crate::FaultContext::reboot) or
    /// [`crash`](crate::FaultContext::crash) does, one scheduler tick from
    /// now: the process's tasks are aborted, its connections reset, and its
    /// storage goes through crash resolution, so every write not yet synced,
    /// including one whose sync is in flight, resolves by crash physics
    /// ([`RebootKind::CrashAndWipe`](crate::RebootKind::CrashAndWipe) also
    /// wipes the disk). It is how a process reaches a crash at an exact
    /// point of its own protocol (mid-append, between two syncs) without a
    /// round trip through a fault injector.
    ///
    /// With `recovery_delay` the process restarts that long after the kill;
    /// without it, it stays down until a fault injector
    /// [restarts](crate::FaultContext::restart) it. No randomness is drawn.
    /// The process counts as dead from the kill until it restarts.
    ///
    /// Call it, then keep awaiting the work it interrupts: the kill lands
    /// before that work's next storage completion.
    ///
    /// # Errors
    ///
    /// [`SimulationError::InvalidState`](crate::SimulationError::InvalidState)
    /// for [`RebootKind::Graceful`](crate::RebootKind::Graceful) (a process
    /// shuts itself down gracefully by returning), and
    /// [`SimulationError::SimulationShutdown`](crate::SimulationError::SimulationShutdown)
    /// once the simulation is gone.
    pub fn crash(
        &self,
        kind: crate::RebootKind,
        recovery_delay: Option<std::time::Duration>,
    ) -> crate::SimulationResult<()> {
        let cause = match kind {
            crate::RebootKind::Crash => crate::sim::ProcessKillKind::Crash,
            crate::RebootKind::CrashAndWipe => crate::sim::ProcessKillKind::CrashAndWipe,
            crate::RebootKind::Graceful => {
                return Err(crate::SimulationError::InvalidState(
                    "crash_self: a graceful shutdown is the process returning".to_string(),
                ));
            }
        };
        crate::assert_reachable!("crash: a process crashed itself");
        let recovery_delay_ms =
            recovery_delay.map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
        self.sim.upgrade()?.schedule_event(
            crate::sim::Event::ProcessForceKill {
                ip: self.ip,
                recovery_delay_ms,
                cause,
            },
            std::time::Duration::from_nanos(1),
        );
        tracing::info!(ip = %self.ip, ?kind, "process crashed itself");
        Ok(())
    }
}
