//! The simulator's side of [`moonpool_buggify::shard!`]: the shard book, the
//! gate every destructive fault asks, and the shard-aware fault shapes
//! (aim, edge and the liveness core).
//!
//! The code under test declares each shard's [`Replication`] with
//! `shard!`. The run keeps every declaration in a [`ShardBook`], read-only
//! for harnesses and oracles through [`SimContext::shards`](crate::SimContext::shards)
//! and [`FaultContext::shards`](crate::FaultContext::shards).
//!
//! # The book
//!
//! Each declaring process (or the driver, for a workload) keeps its latest
//! view of a shard. A view is `current`, plus `next` while a move is in
//! flight. The shard requires every replication of every view not yet
//! superseded. A view is superseded when:
//!
//! - its declarer declares the shard again;
//! - a later declaration resolves its move: a `current` equal to its `next`
//!   (the move is done, and stable views of its old `current` go too) or
//!   to its `current` (the move is aborted);
//! - a later declaration says the shard is gone (`shard!(name)`).
//!
//! A stale view of an old replication stays until its declarer declares
//! again, so it only makes the shard stricter. A process that dies mid-move
//! leaves the shard moving until a replica declares it again.
//!
//! # The gate
//!
//! Before a destructive fault lands, the simulator evaluates each shard the
//! fault touches on what would be left (`FoundationDB`'s `canKillProcesses`,
//! `fdbrpc/sim2.cpp`). A fault that would break a need is downgraded: it
//! still happens at that instant, in a lesser form.
//!
//! | Fault | Judged against | When the need would break |
//! | --- | --- | --- |
//! | kill, hold-down crash | `progress` on the replicas left up | a clean restart in place |
//! | outage | `progress` | only the victims that fit go down |
//! | partition | `progress` on one side | not made |
//! | wipe | `durable` on the replicas left intact | a crash |
//! | disk failure | `durable` | not made |
//! | crash with dirty sectors | `durable` per stripe | a kill that keeps the page cache |
//! | random or scripted damage | `durable` per stripe | not placed on that sector |
//!
//! A replica is up when its process is alive and it is not partitioned from
//! another live replica of the same replication. A replica is intact when it
//! is not wiped (until it heals, see [`moonpool_buggify::shard::caught_up`]),
//! its disk has not failed, and, per stripe, no sector of its copy is
//! damaged. An address the simulator does not know counts as up and intact.
//!
//! Under [`ShardFaults::durable_only`] the gate judges `durable` alone, and
//! kills, outages and partitions are never downgraded.
//!
//! The gate draws no randomness: it reads the book, the dead set, the
//! network and the disks. Every gate evaluation asserts it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_buggify::shard::{Declaration, Need, Replica, Replication, Sink};

use crate::chaos::state_handle::StateHandle;
use crate::providers::ScriptedResolver;
use crate::sim::WeakSimWorld;

/// The run's state key the [`ShardBook`] is published under. The runner
/// publishes a fresh book for every iteration.
pub const SHARD_BOOK_KEY: &str = "moonpool:shard-book";

/// How the aim shapes faults: see [`ShardFaults::aim`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aim {
    factor: f64,
}

impl Aim {
    /// Multiply the attrition rate by `factor` on the replicas of a moving
    /// shard whose declaring site is active this seed. A factor below 1 is
    /// raised to 1.
    #[must_use]
    pub fn moves(factor: f64) -> Self {
        Self {
            factor: factor.max(1.0),
        }
    }

    /// The rate multiplier.
    #[must_use]
    pub fn factor(&self) -> f64 {
        self.factor
    }
}

/// Which replicas the liveness core keeps: see [`ShardFaults::core`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Core {
    /// One minimal set that satisfies `progress`, drawn per shard.
    RandomMinimal,
    /// Every replica alive when the chaos window closes.
    All,
}

/// Shard-aware fault shapes, on top of the gate (which is always on).
///
/// ```rust
/// use moonpool_sim::{Aim, Core, ShardFaults, SimulationBuilder};
///
/// let _builder = SimulationBuilder::new().shard_faults(
///     ShardFaults::default()
///         .aim(Aim::moves(8.0))       // fault rate x8 on a moving shard's replicas
///         .edge(0.2)                  // 20% of draws push a shard to its minimum
///         .core(Core::RandomMinimal), // at chaos end, heal a minimal set per shard
/// );
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShardFaults {
    aim: Option<Aim>,
    edge: f64,
    core: Option<Core>,
    off: BTreeSet<String>,
    all_off: bool,
    durable_only: bool,
}

impl ShardFaults {
    /// Aim faults at moves. Each declaring site that starts a move is
    /// activated once per seed, with probability 0.5. While an active
    /// site's move is in flight, the built-in attrition draws its replicas
    /// `factor` times as often and waits `factor` times less between
    /// rounds. Each active site gets an `assert_sometimes!` that a fault
    /// landed on a replica during its move.
    #[must_use]
    pub fn aim(mut self, aim: Aim) -> Self {
        self.aim = Some(aim);
        self
    }

    /// Push shards to their edge: on this fraction of the built-in
    /// attrition's rounds (per-process scope), the round picks a shard and
    /// takes down every eligible replica outside one minimal set that keeps
    /// `progress`. The gate, not `max_dead`, bounds such a round. Clamped
    /// to `0..=1`.
    #[must_use]
    pub fn edge(mut self, fraction: f64) -> Self {
        self.edge = fraction.clamp(0.0, 1.0);
        self
    }

    /// At the end of the chaos window, keep a liveness core per shard
    /// (`TigerBeetle`'s `transition_to_liveness_mode`): the core's replicas
    /// are restarted if down, made healthy and spared; replicas outside
    /// every core that are down then stay down.
    #[must_use]
    pub fn core(mut self, core: Core) -> Self {
        self.core = Some(core);
        self
    }

    /// Take the shard `name` out of the gate, the aim, the edge and the
    /// core: for a scenario that goes past the shard's tolerance on
    /// purpose. Its declarations are still recorded.
    #[must_use]
    pub fn off_for(mut self, name: impl Into<String>) -> Self {
        self.off.insert(name.into());
        self
    }

    /// Take every shard out of the gate, the aim, the edge and the core:
    /// [`off_for`](Self::off_for) for all of them. Declarations are still
    /// recorded.
    #[must_use]
    pub fn off_all(mut self) -> Self {
        self.all_off = true;
        self
    }

    /// Judge only each shard's `durable` need: kills, hold-downs, outages
    /// and partitions are never downgraded for `progress`, while wipes,
    /// disk failures, a crash's dirty sectors and damage still are. For a
    /// system whose chaos takes a whole quorum down on purpose and checks
    /// that it recovers: unavailability is a fault it must survive, lost
    /// data is not.
    #[must_use]
    pub fn durable_only(mut self) -> Self {
        self.durable_only = true;
        self
    }

    /// Whether the gate judges `progress` (see
    /// [`durable_only`](Self::durable_only)).
    #[must_use]
    pub fn judges_progress(&self) -> bool {
        !self.durable_only
    }

    /// The aim, if set.
    #[must_use]
    pub fn aim_config(&self) -> Option<Aim> {
        self.aim
    }

    /// The edge fraction.
    #[must_use]
    pub fn edge_fraction(&self) -> f64 {
        self.edge
    }

    /// The core, if set.
    #[must_use]
    pub fn core_config(&self) -> Option<Core> {
        self.core
    }

    /// Whether the shard `name` is out of the gate.
    #[must_use]
    pub fn is_off(&self, name: &str) -> bool {
        self.all_off || self.off.contains(name)
    }
}

/// How the gate downgraded one fault, for [`ShardCounts::downgrades`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Downgrade {
    /// A kill or a hold-down crash became a clean restart in place.
    KillToRestart,
    /// A wipe became a crash.
    WipeToCrash,
    /// A crash became a kill that keeps the page cache.
    CrashToKeepCache,
    /// A timed crash (an outage victim) was not made.
    CrashSpared,
    /// A partition was not made.
    PartitionRefused,
    /// A disk failure was not made.
    DiskFailureRefused,
    /// Scripted damage (`corrupt_bytes`) was not placed.
    DamageRefused,
}

/// What the book saw in one run: see [`Shards::counts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShardCounts {
    /// Every `shard!` call.
    pub declarations: u64,
    /// Declarations that started a move.
    pub moves_started: u64,
    /// Moves that ended on their `next`.
    pub moves_done: u64,
    /// Moves that ended back on their `current`.
    pub moves_aborted: u64,
    /// Faults the gate downgraded, by kind.
    pub downgrades: BTreeMap<Downgrade, u64>,
    /// Attrition victims drawn under an active aim.
    pub aimed: u64,
    /// Attrition rounds that pushed a shard to its edge.
    pub edge: u64,
    /// Shards that got a liveness core.
    pub core: u64,
    /// Wiped replicas that healed.
    pub healed: u64,
}

impl ShardCounts {
    /// Every downgrade, of any kind.
    #[must_use]
    pub fn downgrades_total(&self) -> u64 {
        self.downgrades.values().sum()
    }

    /// The downgrades of one kind.
    #[must_use]
    pub fn downgrades_of(&self, kind: Downgrade) -> u64 {
        self.downgrades.get(&kind).copied().unwrap_or(0)
    }
}

/// One `shard!` call, as the book recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardEvent {
    /// When it ran (simulated time).
    pub at: Duration,
    /// The shard's name.
    pub name: String,
    /// The declaring process's IP, or `None` for the driver or a workload.
    pub process: Option<String>,
    /// The call site, `file:line`.
    pub site: &'static str,
    /// The replication declared now, or `None` for a shard that is gone.
    pub current: Option<Replication>,
    /// The replication a move goes to.
    pub next: Option<Replication>,
}

/// One declarer's view of a shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardView {
    /// The declaring process's IP, or `None` for the driver or a workload.
    pub process: Option<String>,
    /// The call site of the declaration.
    pub site: &'static str,
    /// The replication declared now.
    pub current: Replication,
    /// The replication a move goes to.
    pub next: Option<Replication>,
}

/// A shard as the book holds it now: see [`Shards::get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardState {
    /// The shard's name.
    pub name: String,
    /// Every view not yet superseded.
    pub views: Vec<ShardView>,
}

impl ShardState {
    /// Every replication the shard requires now: each view's `current` and
    /// `next`, without duplicates, in view order.
    #[must_use]
    pub fn replications(&self) -> Vec<Replication> {
        let mut out: Vec<Replication> = Vec::new();
        for view in &self.views {
            for replication in std::iter::once(&view.current).chain(view.next.as_ref()) {
                if !out.contains(replication) {
                    out.push(replication.clone());
                }
            }
        }
        out
    }

    /// Whether a move is in flight.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.views.iter().any(|view| view.next.is_some())
    }

    /// Every replica of every required replication, sorted.
    #[must_use]
    pub fn replicas(&self) -> Vec<Replica> {
        let mut out = BTreeSet::new();
        for replication in self.replications() {
            out.extend(replicas_of(&replication));
        }
        out.into_iter().collect()
    }
}

/// Every replica a replication names: its list and both needs.
fn replicas_of(replication: &Replication) -> BTreeSet<Replica> {
    let mut out: BTreeSet<Replica> = replication.replicas.iter().cloned().collect();
    out.extend(replication.progress.replicas());
    out.extend(replication.durable.replicas());
    out
}

/// The process address of `replica`: an IP, or the IP of an `ip:port`.
#[must_use]
pub fn replica_ip(replica: &Replica) -> Option<IpAddr> {
    let address = replica.address();
    address.parse::<IpAddr>().ok().or_else(|| {
        address
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|addr| addr.ip())
    })
}

/// The per-site aim state.
#[derive(Debug, Clone, Copy, Default)]
struct Site {
    /// Whether the site is active this seed, once drawn.
    active: bool,
    /// Whether a fault landed on a replica during its current move.
    landed: bool,
}

/// A wiped replica not yet healed.
#[derive(Debug, Clone, Copy, Default)]
struct Wiped {
    /// Whether a declaration listed it after the wipe.
    listed: bool,
}

#[derive(Debug, Default)]
struct Book {
    config: ShardFaults,
    shards: BTreeMap<String, Vec<View>>,
    events: Vec<ShardEvent>,
    counts: ShardCounts,
    wiped: BTreeMap<IpAddr, Wiped>,
    /// Copies lost for good in one shard only, per shard
    /// ([`ShardBook::note_shard_loss`]).
    lost: ShardLosses,
    sites: BTreeMap<&'static str, Site>,
    core: BTreeMap<String, Vec<Replica>>,
    core_members: BTreeSet<IpAddr>,
    held_down: BTreeSet<IpAddr>,
    /// The name table a replica named by `host:port` is resolved through
    /// ([`ShardBook::resolve_names`]).
    names: Option<ScriptedResolver>,
}

impl Book {
    /// `replica` named by its IP: a `host:port` the name table knows
    /// becomes `ip:port`, its first address. Any other replica is kept.
    fn resolved(&self, replica: &Replica) -> Replica {
        if replica_ip(replica).is_some() {
            return replica.clone();
        }
        let Some(names) = &self.names else {
            return replica.clone();
        };
        let Ok((host, port)) = moonpool_core::split_host_port(replica.address()) else {
            return replica.clone();
        };
        match names.addresses(host).and_then(|ips| ips.first().copied()) {
            Some(ip) => Replica::new(std::net::SocketAddr::new(ip, port).to_string()),
            None => replica.clone(),
        }
    }

    /// `need` with every replica [`resolved`](Self::resolved).
    fn resolved_need(&self, need: Need) -> Need {
        match need {
            Need::AtLeast(count, replicas) => Need::AtLeast(
                count,
                replicas
                    .iter()
                    .map(|replica| self.resolved(replica))
                    .collect(),
            ),
            Need::All(needs) => {
                Need::All(needs.into_iter().map(|n| self.resolved_need(n)).collect())
            }
            Need::Any(needs) => {
                Need::Any(needs.into_iter().map(|n| self.resolved_need(n)).collect())
            }
        }
    }

    /// `replication` with every replica [`resolved`](Self::resolved).
    fn resolved_replication(&self, replication: Replication) -> Replication {
        Replication {
            replicas: replication
                .replicas
                .iter()
                .map(|replica| self.resolved(replica))
                .collect(),
            progress: self.resolved_need(replication.progress),
            durable: self.resolved_need(replication.durable),
        }
    }
}

#[derive(Debug, Clone)]
struct View {
    declarer: Option<IpAddr>,
    site: &'static str,
    current: Replication,
    next: Option<Replication>,
}

/// The run's record of every shard declaration.
///
/// One per iteration, published under [`SHARD_BOOK_KEY`]. Read it through
/// [`Shards`]; the runner and the gate write it.
#[derive(Debug, Clone, Default)]
pub struct ShardBook(Arc<Mutex<Book>>);

impl ShardBook {
    /// A fresh book for a run with `config`.
    pub(crate) fn new(config: ShardFaults) -> Self {
        Self(Arc::new(Mutex::new(Book {
            config,
            ..Book::default()
        })))
    }

    /// The book of the run that owns `state`, if one is published.
    #[must_use]
    pub fn of(state: &StateHandle) -> Option<Self> {
        state.get::<ShardBook>(SHARD_BOOK_KEY)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        self.0.lock().expect("Mutex poisoned: prior task panicked")
    }

    /// Resolve every replica a later declaration names by `host:port`
    /// through `names`, the run's name table: the book holds it as
    /// `ip:port`, so a wipe, a failed disk or damage on that process counts
    /// against it. The gate cannot map a host name to a process, so such a
    /// replica counts as intact whatever happens to it: a system under test
    /// that names its replicas by host installs its table here before it
    /// declares. A name the table does not know is kept as it is.
    pub fn resolve_names(&self, names: ScriptedResolver) {
        self.lock().names = Some(names);
    }

    /// The run's shard fault shapes.
    pub(crate) fn config(&self) -> ShardFaults {
        self.lock().config.clone()
    }

    /// Take `name` (or, with `None`, every shard) out of the gate for the
    /// rest of the run.
    fn switch_off(&self, name: Option<&str>) {
        let mut book = self.lock();
        match name {
            Some(name) => {
                book.config.off.insert(name.to_string());
            }
            None => book.config.all_off = true,
        }
    }

    /// Record one declaration from `declarer` at `at`. `activate` draws a
    /// site's aim activation; it is called only for a site that starts a
    /// move under an aim and was never drawn.
    pub(crate) fn declare(
        &self,
        declarer: Option<IpAddr>,
        at: Duration,
        declaration: Declaration,
        activate: impl FnOnce() -> bool,
    ) {
        let mut book = self.lock();
        let book = &mut *book;
        let declaration = Declaration {
            current: declaration.current.map(|r| book.resolved_replication(r)),
            next: declaration.next.map(|r| book.resolved_replication(r)),
            ..declaration
        };
        book.counts.declarations += 1;
        book.events.push(ShardEvent {
            at,
            name: declaration.name.clone(),
            process: declarer.map(|ip| ip.to_string()),
            site: declaration.site,
            current: declaration.current.clone(),
            next: declaration.next.clone(),
        });
        let Declaration {
            name,
            current,
            next,
            site,
        } = declaration;
        let Some(current) = current else {
            let views = book.shards.remove(&name).unwrap_or_default();
            for view in views.iter().filter(|view| view.next.is_some()) {
                end_move(book, view.site);
            }
            crate::assert_reachable!("shard: a shard is gone");
            return;
        };
        // A declaration lists its replicas: a wiped one listed after its
        // wipe may heal once its code says it caught up.
        for replication in std::iter::once(&current).chain(next.as_ref()) {
            for replica in replicas_of(replication) {
                if let Some(wiped) = replica_ip(&replica).and_then(|ip| book.wiped.get_mut(&ip)) {
                    wiped.listed = true;
                }
            }
        }
        let views = book.shards.entry(name).or_default();
        let previous = views
            .iter()
            .find(|view| view.declarer == declarer)
            .and_then(|view| view.next.clone().map(|next| (view.current.clone(), next)));
        let mut ended: Vec<&'static str> = Vec::new();
        let mut done_from: Vec<Replication> = Vec::new();
        let mut kept = Vec::with_capacity(views.len());
        for view in views.drain(..) {
            if let Some(view_next) = &view.next {
                if *view_next == current {
                    book.counts.moves_done += 1;
                    crate::assert_reachable!("shard: a move is done");
                    done_from.push(view.current.clone());
                    ended.push(view.site);
                    continue;
                }
                if view.current == current && next.as_ref() != Some(view_next) {
                    book.counts.moves_aborted += 1;
                    crate::assert_reachable!("shard: a move is aborted");
                    ended.push(view.site);
                    continue;
                }
            }
            if view.declarer == declarer {
                continue;
            }
            kept.push(view);
        }
        // A done move supersedes the stable views of the replication it
        // left.
        kept.retain(|view| view.next.is_some() || !done_from.contains(&view.current));
        let starts_move = next
            .as_ref()
            .is_some_and(|next| previous.as_ref().is_none_or(|(_, before)| before != next));
        kept.push(View {
            declarer,
            site,
            current,
            next,
        });
        *views = kept;
        for site in ended {
            end_move(book, site);
        }
        if starts_move {
            book.counts.moves_started += 1;
            crate::assert_reachable!("shard: a move starts");
            if book.config.aim.is_some() && !book.sites.contains_key(site) {
                let active = activate();
                book.sites.insert(
                    site,
                    Site {
                        active,
                        landed: false,
                    },
                );
            }
            if let Some(state) = book.sites.get_mut(site) {
                state.landed = false;
            }
        }
    }

    /// The replica at `ip` lost its data: it is not intact until it heals.
    pub(crate) fn note_wipe(&self, ip: IpAddr) {
        self.lock().wiped.insert(ip, Wiped::default());
    }

    /// The replica at `ip` lost its copy of `shard` alone: that copy is
    /// not intact until it heals, and its copies of other shards stay as
    /// they are.
    pub(crate) fn note_shard_loss(&self, ip: IpAddr, shard: &str) {
        self.lock()
            .lost
            .entry(shard.to_string())
            .or_default()
            .insert(ip);
    }

    /// `ip` opened its copy of `shard` intact: a copy lost in that shard
    /// alone is clean again. A wipe stays until the replica catches up.
    /// Returns whether a lost copy healed.
    pub(crate) fn intact(&self, ip: IpAddr, shard: &str) -> bool {
        let mut book = self.lock();
        let healed = book
            .lost
            .get_mut(shard)
            .is_some_and(|lost| lost.remove(&ip));
        if healed {
            book.counts.healed += 1;
        }
        healed
    }

    /// The copies lost in one shard only, per shard.
    pub(crate) fn losses(&self) -> ShardLosses {
        self.lock().lost.clone()
    }

    /// `replica`'s copy of `shard` caught up: it heals if a declaration
    /// listed it after its wipe, or if it lost that shard's copy alone.
    /// Returns whether it healed.
    pub(crate) fn caught_up(&self, shard: &str, replica: &Replica) -> bool {
        let mut book = self.lock();
        let Some(ip) = replica_ip(&book.resolved(replica)) else {
            return false;
        };
        if book
            .lost
            .get_mut(shard)
            .is_some_and(|lost| lost.remove(&ip))
        {
            book.counts.healed += 1;
            return true;
        }
        if book.wiped.get(&ip).is_some_and(|wiped| wiped.listed) {
            book.wiped.remove(&ip);
            book.counts.healed += 1;
            crate::assert_reachable!("shard: a wiped replica heals");
            return true;
        }
        false
    }

    /// The wiped replicas not yet healed.
    pub(crate) fn wiped(&self) -> BTreeSet<IpAddr> {
        self.lock().wiped.keys().copied().collect()
    }

    /// Count one downgrade.
    pub(crate) fn count(&self, kind: Downgrade) {
        *self.lock().counts.downgrades.entry(kind).or_default() += 1;
    }

    /// Count one aimed victim.
    pub(crate) fn count_aimed(&self) {
        self.lock().counts.aimed += 1;
    }

    /// Count one edge round.
    pub(crate) fn count_edge(&self) {
        self.lock().counts.edge += 1;
    }

    /// A destructive fault landed on `ip`: every move in flight that
    /// lists it records a landing.
    pub(crate) fn note_fault(&self, ip: IpAddr) {
        let mut book = self.lock();
        let book = &mut *book;
        if book.config.aim.is_none() {
            return;
        }
        for views in book.shards.values() {
            for view in views {
                let Some(next) = &view.next else {
                    continue;
                };
                let lists = replicas_of(&view.current)
                    .iter()
                    .chain(replicas_of(next).iter())
                    .any(|replica| replica_ip(replica) == Some(ip));
                if lists && let Some(site) = book.sites.get_mut(view.site) {
                    site.landed = true;
                }
            }
        }
    }

    /// Every shard the gate covers, with the replications it requires.
    pub(crate) fn requirements(&self) -> Requirements {
        let book = self.lock();
        book.shards
            .iter()
            .filter(|(name, _)| !book.config.is_off(name))
            .map(|(name, views)| (name.clone(), state_of(name, views).replications()))
            .filter(|(_, replications)| !replications.is_empty())
            .collect()
    }

    /// The processes the active aim points at: every replica of a moving
    /// shard whose declaring site is active.
    pub(crate) fn aimed(&self) -> BTreeSet<IpAddr> {
        let book = self.lock();
        let mut out = BTreeSet::new();
        for (name, views) in &book.shards {
            if book.config.is_off(name) {
                continue;
            }
            for view in views {
                let Some(next) = &view.next else {
                    continue;
                };
                if !book.sites.get(view.site).is_some_and(|site| site.active) {
                    continue;
                }
                for replica in replicas_of(&view.current).iter().chain(&replicas_of(next)) {
                    out.extend(replica_ip(replica));
                }
            }
        }
        out
    }

    /// Record the liveness core.
    pub(crate) fn set_core(
        &self,
        core: BTreeMap<String, Vec<Replica>>,
        members: BTreeSet<IpAddr>,
        held_down: BTreeSet<IpAddr>,
    ) {
        let mut book = self.lock();
        book.counts.core += core.len() as u64;
        book.core = core;
        book.core_members = members;
        book.held_down = held_down;
    }

    /// Whether a scheduled restart of `ip` must be dropped: a replica
    /// outside every core stays down, and a core replica already running
    /// keeps its boot.
    pub(crate) fn drops_restart(&self, ip: IpAddr, running: bool) -> bool {
        let book = self.lock();
        if running && book.core_members.contains(&ip) {
            crate::assert_reachable!("shard core: a stale restart of a core replica is dropped");
            return true;
        }
        if !running && book.held_down.contains(&ip) {
            crate::assert_reachable!("shard core: a replica outside the core stays down");
            return true;
        }
        false
    }

    fn snapshot(&self) -> BookSnapshot {
        let book = self.lock();
        BookSnapshot {
            shards: book
                .shards
                .iter()
                .map(|(name, views)| (name.clone(), state_of(name, views)))
                .collect(),
            events: book.events.clone(),
            counts: book.counts.clone(),
            core: book.core.clone(),
        }
    }
}

/// The book's sites: at a move's end, the aim's per-site assertion.
fn end_move(book: &mut Book, site: &'static str) {
    if book.config.aim.is_none() {
        return;
    }
    if let Some(state) = book.sites.get_mut(site)
        && state.active
    {
        crate::assert_sometimes!(
            state.landed,
            format!("shard: a fault landed during the move at {site}")
        );
        state.landed = false;
    }
}

fn state_of(name: &str, views: &[View]) -> ShardState {
    ShardState {
        name: name.to_string(),
        views: views
            .iter()
            .map(|view| ShardView {
                process: view.declarer.map(|ip| ip.to_string()),
                site: view.site,
                current: view.current.clone(),
                next: view.next.clone(),
            })
            .collect(),
    }
}

struct BookSnapshot {
    shards: BTreeMap<String, ShardState>,
    events: Vec<ShardEvent>,
    counts: ShardCounts,
    core: BTreeMap<String, Vec<Replica>>,
}

/// Every covered shard and the replications it requires.
pub(crate) type Requirements = Vec<(String, Vec<Replication>)>;

/// The read-only view of the run's shards, for harnesses and oracles.
///
/// A harness checks each [event](Self::events) against its own model, so a
/// wrong declaration fails the seed instead of weakening the gate.
#[derive(Debug, Clone)]
pub struct Shards {
    book: ShardBook,
    world: WeakSimWorld,
}

impl Shards {
    pub(crate) fn new(book: ShardBook, world: WeakSimWorld) -> Self {
        Self { book, world }
    }

    /// The shard `name` as the book holds it now, if it is declared.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<ShardState> {
        self.book.snapshot().shards.remove(name)
    }

    /// The names of every shard declared now, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.book.snapshot().shards.into_keys().collect()
    }

    /// Every declaration of the run, oldest first, with its site and process.
    #[must_use]
    pub fn events(&self) -> Vec<ShardEvent> {
        self.book.snapshot().events
    }

    /// What the book and the gate did so far.
    #[must_use]
    pub fn counts(&self) -> ShardCounts {
        self.book.snapshot().counts
    }

    /// Take the shard `name` out of the gate, the aim, the edge and the
    /// core for the rest of the run, as [`ShardFaults::off_for`] does from
    /// the start: for a scenario that decides per seed, mid-run, to go past
    /// the shard's tolerance on purpose. Its declarations are still
    /// recorded.
    pub fn switch_off(&self, name: &str) {
        self.book.switch_off(Some(name));
    }

    /// [`switch_off`](Self::switch_off) for every shard, those declared
    /// later included.
    pub fn switch_off_all(&self) {
        self.book.switch_off(None);
    }

    /// Whether the gate is off for `name`: switched off, or off in the
    /// run's [`ShardFaults`]. An oracle that reads [`clean_copies`](Self::clean_copies)
    /// as the gate's promise skips such a shard.
    #[must_use]
    pub fn is_off(&self, name: &str) -> bool {
        self.book.lock().config.is_off(name)
    }

    /// The liveness core of `name`, once the chaos window closed under
    /// [`ShardFaults::core`].
    #[must_use]
    pub fn core(&self, name: &str) -> Option<Vec<Replica>> {
        self.book.snapshot().core.remove(name)
    }

    /// [`ShardBook::resolve_names`] on the run's book.
    pub fn resolve_names(&self, names: ScriptedResolver) {
        self.book.resolve_names(names);
    }

    /// Say that `ip`'s copy of `shard` opened intact, as
    /// [`moonpool_buggify::shard::intact`] does from the code under test on
    /// that process. Returns whether a copy lost in that shard healed.
    #[must_use = "says whether a lost copy healed"]
    pub fn intact(&self, ip: IpAddr, shard: &str) -> bool {
        self.book.intact(ip, shard)
    }

    /// Say that `replica`'s copy of `shard` caught up, as
    /// [`moonpool_buggify::shard::caught_up`] does from the code under test.
    /// Returns whether a wiped replica healed.
    #[must_use = "says whether a wiped replica healed"]
    pub fn caught_up(&self, shard: &str, replica: &Replica) -> bool {
        self.book.caught_up(shard, replica)
    }

    /// The replicas of `shard` whose copy of `stripe` is clean now, from the
    /// disks' ground truth: not wiped (or healed), disk not failed, and no
    /// damaged sector in a region the replica publishes for
    /// `(shard, stripe)`. Sorted.
    #[must_use]
    pub fn clean_copies(&self, shard: &str, stripe: u64) -> Vec<Replica> {
        let Some(state) = self.get(shard) else {
            return Vec::new();
        };
        let Ok(world) = self.world.upgrade() else {
            return Vec::new();
        };
        let wiped = self.book.wiped();
        let lost = self.book.losses().remove(shard).unwrap_or_default();
        state
            .replicas()
            .into_iter()
            .filter(|replica| {
                replica_ip(replica).is_none_or(|ip| {
                    !wiped.contains(&ip)
                        && !lost.contains(&ip)
                        && world.shard_copy_intact(ip, shard, Some(stripe))
                })
            })
            .collect()
    }
}

/// The simulator's [`Sink`]: records every declaration in the run's book.
pub(crate) struct ShardSink {
    book: ShardBook,
    world: WeakSimWorld,
}

impl ShardSink {
    pub(crate) fn new(book: ShardBook, world: WeakSimWorld) -> Self {
        Self { book, world }
    }
}

impl Sink for ShardSink {
    fn declare(&self, declaration: Declaration) {
        let at = self
            .world
            .upgrade()
            .map_or(Duration::ZERO, |world| world.current_time());
        let declarer = crate::executor::current_owner();
        tracing::debug!(
            shard = %declaration.name,
            site = declaration.site,
            process = ?declarer,
            moving = declaration.next.is_some(),
            "shard declared"
        );
        self.book.declare(declarer, at, declaration, || {
            crate::sim::sim_random_bool(0.5)
        });
    }

    fn caught_up(&self, shard: &str, replica: &Replica) {
        self.book.caught_up(shard, replica);
    }

    fn intact(&self, shard: &str) {
        if let Some(ip) = crate::executor::current_owner() {
            self.book.intact(ip, shard);
        }
    }
}

impl crate::sim::SimWorld {
    /// Install a fresh shard book for a hand-driven world (no
    /// [`SimulationBuilder`](crate::SimulationBuilder) run): `shard!` calls
    /// on this thread land in it, and this world's disks place damage by
    /// it. Returns the read-only view. The runner does this for every
    /// iteration on its own; remove the sink with
    /// [`moonpool_buggify::shard::clear_sink`] when done.
    #[must_use = "the view reads the book"]
    pub fn install_shard_book(&self, faults: ShardFaults) -> Shards {
        let book = ShardBook::new(faults);
        self.set_shard_book(Some(book.clone()));
        moonpool_buggify::shard::set_sink(Box::new(ShardSink::new(book.clone(), self.downgrade())));
        Shards::new(book, self.downgrade())
    }
}

/// Whether `ip` cannot reach another live replica among `peers`.
fn is_cut(
    ip: IpAddr,
    peers: &[IpAddr],
    dead: &BTreeSet<IpAddr>,
    cut: &dyn Fn(IpAddr, IpAddr) -> bool,
) -> bool {
    peers
        .iter()
        .any(|peer| *peer != ip && !dead.contains(peer) && cut(ip, *peer))
}

/// The process IPs a replication names.
fn ips_of(replication: &Replication) -> Vec<IpAddr> {
    replicas_of(replication)
        .iter()
        .filter_map(replica_ip)
        .collect()
}

/// Whether `need` holds when the replicas `unavailable` names are missing;
/// an address the simulator does not know is always available.
fn holds_without(need: &Need, unavailable: &dyn Fn(IpAddr) -> bool) -> bool {
    need.holds(&|replica| replica_ip(replica).is_none_or(|ip| !unavailable(ip)))
}

/// Whether taking `victims` down keeps `progress` of every replication that
/// lists one of them. `cut(a, b)` says whether `a` cannot reach `b` now.
pub(crate) fn down_ok(
    requirements: &Requirements,
    dead: &BTreeSet<IpAddr>,
    victims: &[IpAddr],
    cut: &dyn Fn(IpAddr, IpAddr) -> bool,
) -> bool {
    requirements.iter().all(|(_, replications)| {
        replications.iter().all(|replication| {
            let ips = ips_of(replication);
            if !ips.iter().any(|ip| victims.contains(ip)) {
                return true;
            }
            holds_without(&replication.progress, &|ip| {
                dead.contains(&ip) || victims.contains(&ip) || is_cut(ip, &ips, dead, cut)
            })
        })
    })
}

/// Copies lost for good in one shard only: per shard name, the replicas
/// whose copy of it is gone while their other copies stay.
pub(crate) type ShardLosses = BTreeMap<String, BTreeSet<IpAddr>>;

/// Whether losing the copies of `victims` keeps `durable` of every
/// replication that lists one of them, with `not_intact` lost already in
/// every shard and `losses` in theirs.
pub(crate) fn lose_ok(
    requirements: &Requirements,
    not_intact: &BTreeSet<IpAddr>,
    losses: &ShardLosses,
    victims: &[IpAddr],
) -> bool {
    requirements.iter().all(|(name, replications)| {
        let lost = losses.get(name);
        replications.iter().all(|replication| {
            if !ips_of(replication).iter().any(|ip| victims.contains(ip)) {
                return true;
            }
            holds_without(&replication.durable, &|ip| {
                not_intact.contains(&ip)
                    || lost.is_some_and(|lost| lost.contains(&ip))
                    || victims.contains(&ip)
            })
        })
    })
}

/// Whether splitting `a` from `b` keeps, for every replication with
/// replicas on both sides, `progress` on one side. An empty `b` cuts `a`
/// off from everyone (an asymmetric partition of `a`).
pub(crate) fn split_ok(
    requirements: &Requirements,
    dead: &BTreeSet<IpAddr>,
    a: &[IpAddr],
    b: &[IpAddr],
    cut: &dyn Fn(IpAddr, IpAddr) -> bool,
) -> bool {
    if b.is_empty() {
        return down_ok(requirements, dead, a, cut);
    }
    requirements.iter().all(|(_, replications)| {
        replications.iter().all(|replication| {
            let ips = ips_of(replication);
            if !ips.iter().any(|ip| a.contains(ip)) || !ips.iter().any(|ip| b.contains(ip)) {
                return true;
            }
            let lost = |ip: IpAddr| dead.contains(&ip) || is_cut(ip, &ips, dead, cut);
            holds_without(&replication.progress, &|ip| lost(ip) || b.contains(&ip))
                || holds_without(&replication.progress, &|ip| lost(ip) || a.contains(&ip))
        })
    })
}

/// Run `gate` and assert it drew no randomness.
pub(crate) fn pure<T>(gate: impl FnOnce() -> T) -> T {
    let before = crate::sim::rng_call_count();
    let out = gate();
    crate::assert_always!(
        crate::sim::rng_call_count() == before,
        "shard gate: draws no randomness"
    );
    out
}

/// The network's partition guard for one run: random partitions ask it.
pub(crate) fn partition_guard(
    book: ShardBook,
    dead: crate::runner::fault_injector::DeadSet,
) -> crate::network::PartitionGuard {
    Arc::new(
        move |a: &[IpAddr], b: &[IpAddr], cut: &dyn Fn(IpAddr, IpAddr) -> bool| {
            if !book.config().judges_progress() {
                return true;
            }
            let requirements = book.requirements();
            if requirements.is_empty() {
                return true;
            }
            let dead = dead
                .lock()
                .expect("Mutex poisoned: prior task panicked")
                .clone();
            let ok = pure(|| split_ok(&requirements, &dead, a, b, cut));
            if ok {
                for ip in a.iter().chain(b) {
                    book.note_fault(*ip);
                }
            } else {
                crate::assert_reachable!("shard gate: a random partition is refused");
                book.count(Downgrade::PartitionRefused);
            }
            ok
        },
    )
}

/// The liveness core at the end of the chaos window: see
/// [`ShardFaults::core`]. Returns the core replicas to restart (those
/// dead now) and the ones to make healthy.
pub(crate) fn transition_to_core(
    book: &ShardBook,
    dead: &BTreeSet<IpAddr>,
) -> Option<(Vec<IpAddr>, Vec<IpAddr>)> {
    let kind = book.config().core?;
    let requirements = book.requirements();
    let mut core = BTreeMap::new();
    let mut members = BTreeSet::new();
    let mut replicas_all = BTreeSet::new();
    for (name, replications) in &requirements {
        let all: BTreeSet<Replica> = replications.iter().flat_map(replicas_of).collect();
        replicas_all.extend(all.iter().filter_map(replica_ip));
        let chosen: Vec<Replica> = match kind {
            Core::All => all
                .iter()
                .filter(|replica| replica_ip(replica).is_none_or(|ip| !dead.contains(&ip)))
                .cloned()
                .collect(),
            Core::RandomMinimal => {
                let need = Need::All(
                    replications
                        .iter()
                        .map(|replication| replication.progress.clone())
                        .collect(),
                );
                let sets = need.minimal_sets(&|_| true);
                if sets.is_empty() {
                    all.iter().cloned().collect()
                } else {
                    sets[crate::sim::sim_random_range(0..sets.len())].clone()
                }
            }
        };
        members.extend(chosen.iter().filter_map(replica_ip));
        core.insert(name.clone(), chosen);
    }
    let held_down: BTreeSet<IpAddr> = replicas_all.difference(&members).copied().collect();
    let restart: Vec<IpAddr> = members
        .iter()
        .filter(|ip| dead.contains(ip))
        .copied()
        .collect();
    let healthy: Vec<IpAddr> = members.iter().copied().collect();
    if !core.is_empty() {
        crate::assert_reachable!("shard core: the chaos window closes on a liveness core");
    }
    book.set_core(core, members, held_down);
    Some((restart, healthy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replicas(ips: &[&str]) -> Vec<Replica> {
        ips.iter().map(|ip| Replica::new(*ip)).collect()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test IP")
    }

    fn declaration(
        name: &str,
        current: Option<Replication>,
        next: Option<Replication>,
        site: &'static str,
    ) -> Declaration {
        Declaration {
            name: name.to_string(),
            current,
            next,
            site,
        }
    }

    fn r(ips: &[&str]) -> Replication {
        Replication::tolerates(replicas(ips), 1, 1)
    }

    fn requirements(book: &ShardBook) -> Requirements {
        book.requirements()
    }

    #[test]
    fn a_shard_switched_off_leaves_the_gate() {
        let book = ShardBook::new(ShardFaults::default());
        let a = Some(ip("10.0.0.1"));
        for name in ["orders", "users"] {
            book.declare(
                a,
                Duration::ZERO,
                declaration(name, Some(r(&["10.0.0.1", "10.0.0.2"])), None, "s:1"),
                || false,
            );
        }
        assert_eq!(requirements(&book).len(), 2);
        book.switch_off(Some("orders"));
        let left: Vec<String> = requirements(&book)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(left, vec!["users".to_string()]);
        book.switch_off(None);
        assert!(requirements(&book).is_empty());
        assert!(ShardFaults::default().off_all().is_off("anything"));
        assert!(ShardFaults::default().judges_progress());
        assert!(!ShardFaults::default().durable_only().judges_progress());
    }

    #[test]
    fn a_move_requires_both_until_it_is_done() {
        let book = ShardBook::new(ShardFaults::default());
        let a = Some(ip("10.0.0.1"));
        let r1 = r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        let r2 = r(&["10.0.0.2", "10.0.0.3", "10.0.0.4"]);
        book.declare(
            a,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), None, "s:1"),
            || true,
        );
        book.declare(
            a,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), Some(r2.clone()), "s:2"),
            || true,
        );
        assert_eq!(requirements(&book)[0].1, vec![r1.clone(), r2.clone()]);
        book.declare(
            a,
            Duration::ZERO,
            declaration("s", Some(r2.clone()), None, "s:3"),
            || true,
        );
        assert_eq!(requirements(&book)[0].1, vec![r2]);
        let counts = book.snapshot().counts;
        assert_eq!((counts.moves_started, counts.moves_done), (1, 1));
    }

    #[test]
    fn an_aborted_move_returns_to_current() {
        let book = ShardBook::new(ShardFaults::default());
        let a = Some(ip("10.0.0.1"));
        let r1 = r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        let r2 = r(&["10.0.0.4", "10.0.0.5", "10.0.0.6"]);
        book.declare(
            a,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), Some(r2), "s:2"),
            || true,
        );
        book.declare(
            a,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), None, "s:3"),
            || true,
        );
        assert_eq!(requirements(&book)[0].1, vec![r1]);
        assert_eq!(book.snapshot().counts.moves_aborted, 1);
    }

    #[test]
    fn a_dead_declarer_leaves_the_move_until_another_replica_resolves_it() {
        let book = ShardBook::new(ShardFaults::default());
        let r1 = r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        let r2 = r(&["10.0.0.2", "10.0.0.3", "10.0.0.4"]);
        let leader = Some(ip("10.0.0.1"));
        let other = Some(ip("10.0.0.2"));
        book.declare(
            other,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), None, "s:1"),
            || true,
        );
        book.declare(
            leader,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), Some(r2.clone()), "s:2"),
            || true,
        );
        // The leader dies here: nothing changes until a replica declares.
        assert!(book.snapshot().shards["s"].is_moving());
        book.declare(
            other,
            Duration::ZERO,
            declaration("s", Some(r2.clone()), None, "s:3"),
            || true,
        );
        let state = book.snapshot().shards.remove("s").expect("declared");
        assert!(!state.is_moving());
        assert_eq!(state.replications(), vec![r2]);
    }

    #[test]
    fn a_stale_view_only_makes_the_shard_stricter() {
        let book = ShardBook::new(ShardFaults::default());
        let r1 = r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
        let r2 = r(&["10.0.0.4", "10.0.0.5", "10.0.0.6"]);
        book.declare(
            Some(ip("10.0.0.4")),
            Duration::ZERO,
            declaration("s", Some(r2.clone()), None, "s:1"),
            || true,
        );
        book.declare(
            Some(ip("10.0.0.1")),
            Duration::ZERO,
            declaration("s", Some(r1.clone()), None, "s:1"),
            || true,
        );
        assert_eq!(requirements(&book)[0].1, vec![r2.clone(), r1]);
        // The stale declarer catches up.
        book.declare(
            Some(ip("10.0.0.1")),
            Duration::ZERO,
            declaration("s", Some(r2.clone()), None, "s:1"),
            || true,
        );
        assert_eq!(requirements(&book)[0].1, vec![r2]);
    }

    #[test]
    fn gone_clears_the_shard_and_off_hides_it() {
        let book = ShardBook::new(ShardFaults::default().off_for("hidden"));
        let r1 = r(&["10.0.0.1"]);
        book.declare(
            None,
            Duration::ZERO,
            declaration("s", Some(r1.clone()), None, "s:1"),
            || true,
        );
        book.declare(
            None,
            Duration::ZERO,
            declaration("hidden", Some(r1), None, "s:1"),
            || true,
        );
        assert_eq!(requirements(&book).len(), 1);
        book.declare(
            None,
            Duration::ZERO,
            declaration("s", None, None, "s:4"),
            || true,
        );
        assert!(requirements(&book).is_empty());
        assert_eq!(book.snapshot().events.len(), 3);
    }

    #[test]
    fn the_gate_keeps_a_majority() {
        let reqs: Requirements = vec![("s".into(), vec![r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"])])];
        let none = |_: IpAddr, _: IpAddr| false;
        let mut dead = BTreeSet::new();
        assert!(down_ok(&reqs, &dead, &[ip("10.0.0.1")], &none));
        dead.insert(ip("10.0.0.1"));
        assert!(!down_ok(&reqs, &dead, &[ip("10.0.0.2")], &none));
        assert!(
            down_ok(&reqs, &dead, &[ip("10.0.0.9")], &none),
            "not a replica"
        );
        // A partition between the two live replicas counts as a cut.
        dead.clear();
        let cut = |a: IpAddr, b: IpAddr| {
            let pair = [ip("10.0.0.2"), ip("10.0.0.3")];
            pair.contains(&a) && pair.contains(&b)
        };
        assert!(!down_ok(&reqs, &dead, &[ip("10.0.0.1")], &cut));
    }

    #[test]
    fn the_gate_judges_wipes_on_intact_copies() {
        let reqs: Requirements = vec![("s".into(), vec![r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"])])];
        let mut lost = BTreeSet::new();
        assert!(lose_ok(
            &reqs,
            &lost,
            &ShardLosses::new(),
            &[ip("10.0.0.1")]
        ));
        lost.insert(ip("10.0.0.1"));
        assert!(!lose_ok(
            &reqs,
            &lost,
            &ShardLosses::new(),
            &[ip("10.0.0.2")]
        ));
        assert!(
            lose_ok(&reqs, &lost, &ShardLosses::new(), &[ip("10.0.0.1")]),
            "re-wiping loses nothing more"
        );
    }

    #[test]
    fn a_split_keeps_progress_on_one_side() {
        let reqs: Requirements = vec![("s".into(), vec![r(&["10.0.0.1", "10.0.0.2", "10.0.0.3"])])];
        let none = |_: IpAddr, _: IpAddr| false;
        let dead = BTreeSet::new();
        assert!(split_ok(
            &reqs,
            &dead,
            &[ip("10.0.0.1")],
            &[ip("10.0.0.2"), ip("10.0.0.3")],
            &none
        ));
        let mut one_dead = BTreeSet::new();
        one_dead.insert(ip("10.0.0.3"));
        assert!(!split_ok(
            &reqs,
            &one_dead,
            &[ip("10.0.0.1")],
            &[ip("10.0.0.2")],
            &none
        ));
        assert!(!split_ok(&reqs, &one_dead, &[ip("10.0.0.1")], &[], &none));
    }

    #[test]
    fn healing_needs_a_listing_after_the_wipe() {
        let book = ShardBook::new(ShardFaults::default());
        let replica = Replica::new("10.0.0.1");
        book.note_wipe(ip("10.0.0.1"));
        assert!(!book.caught_up("s", &replica), "not listed since the wipe");
        book.declare(
            None,
            Duration::ZERO,
            declaration("s", Some(r(&["10.0.0.1"])), None, "s:1"),
            || true,
        );
        assert!(book.caught_up("s", &replica));
        assert!(book.wiped().is_empty());
    }

    #[test]
    fn replica_addresses_may_carry_a_port() {
        assert_eq!(
            replica_ip(&Replica::new("10.0.0.1:4500")),
            Some(ip("10.0.0.1"))
        );
        assert_eq!(replica_ip(&Replica::new("node-a")), None);
    }

    #[test]
    fn a_replica_named_by_host_counts_by_its_ip() {
        let book = ShardBook::new(ShardFaults::default());
        let table = ScriptedResolver::new();
        table.set("db-1", vec![ip("10.0.0.1")]);
        book.lock().names = Some(table);
        let named = |hosts: &[&str]| {
            Replication::tolerates(
                hosts
                    .iter()
                    .map(|h| Replica::new(format!("{h}:4500")))
                    .collect(),
                1,
                1,
            )
        };
        book.declare(
            None,
            Duration::ZERO,
            declaration("s", Some(named(&["db-1", "10.0.0.2", "db-3"])), None, "s:1"),
            || true,
        );
        let reqs = requirements(&book);
        let replicas = reqs[0].1[0].replicas.clone();
        assert_eq!(
            replicas,
            vec![
                Replica::new("10.0.0.1:4500"),
                Replica::new("10.0.0.2:4500"),
                Replica::new("db-3:4500"),
            ],
            "a known name becomes its IP; an unknown one stays"
        );
        // The wipe of the named process now counts against the shard.
        let mut lost = BTreeSet::new();
        lost.insert(ip("10.0.0.1"));
        assert!(!lose_ok(
            &reqs,
            &lost,
            &ShardLosses::new(),
            &[ip("10.0.0.2")]
        ));
        // A heal names the process as the code does, by its host.
        book.note_wipe(ip("10.0.0.1"));
        book.declare(
            None,
            Duration::ZERO,
            declaration("s", Some(named(&["db-1", "10.0.0.2", "db-3"])), None, "s:1"),
            || true,
        );
        assert!(book.caught_up("s", &Replica::new("db-1:4500")));
    }
}
