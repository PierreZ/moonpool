//! Shards: the code under test declares how its data is replicated, and the
//! simulator keeps every fault it draws survivable.
//!
//! ```
//! use moonpool_buggify::shard;
//! use moonpool_buggify::shard::{Replica, Replication};
//!
//! let replicas: Vec<Replica> = ["10.0.1.1", "10.0.1.2", "10.0.1.3"]
//!     .into_iter()
//!     .map(Replica::new)
//!     .collect();
//! let r1 = Replication::tolerates(replicas, 1, 1);
//!
//! shard!("journal/orders", current = r1.clone()); // stable
//! # let r2 = r1.clone();
//! shard!("journal/orders", current = r1, next = r2.clone()); // moving
//! shard!("journal/orders", current = r2); // the move is done
//! shard!("journal/orders"); // gone
//! ```
//!
//! A shard is a named piece of data that several replicas hold. Its
//! [`Replication`] says which replicas hold it and what the system needs to
//! keep working ([`Replication::progress`]) and to keep the data
//! ([`Replication::durable`]). A [`Need`] is data, not a closure: the
//! simulator evaluates it, finds its minimal sets, and prints it.
//!
//! A declaration with `next` starts a **move**: until a later declaration
//! without `next`, the shard must hold on `current` **and** on `next`. The
//! call site (`file:line`) names the move, as for [`buggify!`](crate::buggify!).
//!
//! Several replicas may declare one shard, and a stale replica may declare an
//! old view. The simulator keeps every replication not yet superseded and
//! requires all of them, so a stale view only makes the shard stricter.
//!
//! Like [`hint!`](crate::hint!), a declaration is **inert** outside a
//! simulation: no sink is installed, the arguments are not evaluated, and
//! nothing is recorded.
//!
//! The simulator's side is a [`Sink`], installed per thread with
//! [`set_sink`] for one run.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;

/// One replica of a shard: an address the simulator knows, such as a
/// process IP (`"10.0.1.1"`) or an IP and port (`"10.0.1.1:4500"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Replica(pub String);

impl Replica {
    /// The replica at `address`.
    #[must_use]
    pub fn new(address: impl Into<String>) -> Self {
        Self(address.into())
    }

    /// The address, as given.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Replica {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a shard needs from its replicas, as data.
///
/// - Raft: `AtLeast(n / 2 + 1, replicas)`.
/// - `FoundationDB` triple replication over zones: `AtLeast(3, ...)` over one
///   replica per zone.
/// - Partitions of `2F + 1` acceptors: `All` of a majority per partition
///   for an election, `Any` of them for a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// At least this many of these replicas.
    AtLeast(usize, Vec<Replica>),
    /// Every one of these needs.
    All(Vec<Need>),
    /// One of these needs, or more.
    Any(Vec<Need>),
}

/// The most minimal sets [`Need::minimal_sets`] returns. A need with more
/// is cut at this count, in a stable order.
pub const MAX_MINIMAL_SETS: usize = 1024;

impl Need {
    /// A majority of `replicas`: `AtLeast(n / 2 + 1, replicas)`.
    #[must_use]
    pub fn majority(replicas: Vec<Replica>) -> Self {
        let count = replicas.len() / 2 + 1;
        Self::AtLeast(count, replicas)
    }

    /// Whether the need holds when exactly the replicas for which `ok`
    /// answers `true` are available. `AtLeast(0, _)` and an empty `All`
    /// always hold; an empty `Any` never holds.
    pub fn holds(&self, ok: &dyn Fn(&Replica) -> bool) -> bool {
        match self {
            Self::AtLeast(count, replicas) => {
                let mut seen = BTreeSet::new();
                replicas
                    .iter()
                    .filter(|replica| ok(replica) && seen.insert(*replica))
                    .count()
                    >= *count
            }
            Self::All(needs) => needs.iter().all(|need| need.holds(ok)),
            Self::Any(needs) => needs.iter().any(|need| need.holds(ok)),
        }
    }

    /// Every replica this need names, sorted and without duplicates.
    #[must_use]
    pub fn replicas(&self) -> Vec<Replica> {
        let mut out = BTreeSet::new();
        self.collect(&mut out);
        out.into_iter().collect()
    }

    fn collect(&self, out: &mut BTreeSet<Replica>) {
        match self {
            Self::AtLeast(_, replicas) => out.extend(replicas.iter().cloned()),
            Self::All(needs) | Self::Any(needs) => {
                for need in needs {
                    need.collect(out);
                }
            }
        }
    }

    /// The minimal sets of replicas, among those `ok` admits, that make the
    /// need hold: no set holds a smaller set that also makes it hold.
    /// Sorted, each set sorted, at most [`MAX_MINIMAL_SETS`]. Empty when no
    /// set of admitted replicas makes the need hold.
    #[must_use]
    pub fn minimal_sets(&self, ok: &dyn Fn(&Replica) -> bool) -> Vec<Vec<Replica>> {
        let sets = self.sets(ok);
        let mut minimal: Vec<BTreeSet<Replica>> = Vec::new();
        let mut sorted = sets;
        sorted.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        for set in sorted {
            if !minimal.iter().any(|kept| kept.is_subset(&set)) {
                minimal.push(set);
            }
        }
        let mut out: Vec<Vec<Replica>> = minimal
            .into_iter()
            .map(|set| set.into_iter().collect())
            .collect();
        out.sort();
        out.truncate(MAX_MINIMAL_SETS);
        out
    }

    /// Sets that make the need hold (not all minimal), capped.
    fn sets(&self, ok: &dyn Fn(&Replica) -> bool) -> Vec<BTreeSet<Replica>> {
        match self {
            Self::AtLeast(count, replicas) => {
                let pool: Vec<Replica> = replicas
                    .iter()
                    .filter(|replica| ok(replica))
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                combinations(&pool, *count)
            }
            Self::All(needs) => {
                let mut acc: Vec<BTreeSet<Replica>> = vec![BTreeSet::new()];
                for need in needs {
                    let options = need.sets(ok);
                    let mut next = Vec::new();
                    'outer: for base in &acc {
                        for option in &options {
                            let mut joined = base.clone();
                            joined.extend(option.iter().cloned());
                            next.push(joined);
                            if next.len() >= MAX_MINIMAL_SETS * 4 {
                                break 'outer;
                            }
                        }
                    }
                    acc = next;
                    if acc.is_empty() {
                        break;
                    }
                }
                acc
            }
            Self::Any(needs) => {
                let mut out = Vec::new();
                for need in needs {
                    out.extend(need.sets(ok));
                    if out.len() >= MAX_MINIMAL_SETS * 4 {
                        break;
                    }
                }
                out
            }
        }
    }
}

/// Every `k`-subset of `pool`, in lexicographic order, capped.
fn combinations(pool: &[Replica], k: usize) -> Vec<BTreeSet<Replica>> {
    if k > pool.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut picks: Vec<usize> = (0..k).collect();
    loop {
        out.push(picks.iter().map(|&at| pool[at].clone()).collect());
        if out.len() >= MAX_MINIMAL_SETS * 4 {
            return out;
        }
        // Advance to the next combination.
        let Some(at) = (0..k).rev().find(|&at| picks[at] < pool.len() - k + at) else {
            return out;
        };
        picks[at] += 1;
        for after in at + 1..k {
            picks[after] = picks[after - 1] + 1;
        }
    }
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, needs: &[Need]| -> fmt::Result {
            for (at, need) in needs.iter().enumerate() {
                if at > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{need}")?;
            }
            Ok(())
        };
        match self {
            Self::AtLeast(count, replicas) => {
                write!(f, "at least {count} of [")?;
                for (at, replica) in replicas.iter().enumerate() {
                    if at > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{replica}")?;
                }
                f.write_str("]")
            }
            Self::All(needs) => {
                f.write_str("all of (")?;
                list(f, needs)?;
                f.write_str(")")
            }
            Self::Any(needs) => {
                f.write_str("any of (")?;
                list(f, needs)?;
                f.write_str(")")
            }
        }
    }
}

/// How a shard is replicated: the replicas, and what it needs from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replication {
    /// Every replica that holds a copy.
    pub replicas: Vec<Replica>,
    /// What the shard needs to make progress, judged on the replicas that
    /// are up and reachable.
    pub progress: Need,
    /// What the shard needs to keep its data, judged on the replicas whose
    /// copy is intact.
    pub durable: Need,
}

impl Replication {
    /// A count-shaped replication: the shard makes progress while at most
    /// `down` replicas are down, and keeps its data while at most `lost`
    /// copies are lost. Both are clamped to the replica count.
    #[must_use]
    pub fn tolerates(replicas: Vec<Replica>, down: usize, lost: usize) -> Self {
        let count = replicas.len();
        Self {
            progress: Need::AtLeast(count - down.min(count), replicas.clone()),
            durable: Need::AtLeast(count - lost.min(count), replicas.clone()),
            replicas,
        }
    }
}

impl fmt::Display for Replication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "progress: {}; durable: {}", self.progress, self.durable)
    }
}

/// One `shard!` call, as the sink receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The shard's name.
    pub name: String,
    /// The replication now, or `None` for a shard that is gone.
    pub current: Option<Replication>,
    /// The replication a move goes to, if the shard is moving.
    pub next: Option<Replication>,
    /// The call site, `file:line`.
    pub site: &'static str,
}

/// The simulator's side of the shards, installed per thread for a run.
pub trait Sink {
    /// A `shard!` call ran.
    fn declare(&self, declaration: Declaration);
    /// The code says `replica`'s copy of `shard` has caught up after a wipe
    /// (see [`caught_up`]).
    fn caught_up(&self, shard: &str, replica: &Replica);
    /// The calling process opened its copy of `shard` and found it intact
    /// (see [`intact`]).
    fn intact(&self, shard: &str);
}

thread_local! {
    static SINK: RefCell<Option<Box<dyn Sink>>> = RefCell::new(None);
}

/// Install the simulator's sink on this thread for one run.
pub fn set_sink(sink: Box<dyn Sink>) {
    SINK.with(|slot| *slot.borrow_mut() = Some(sink));
}

/// Remove the sink: every declaration is inert again.
pub fn clear_sink() {
    SINK.with(|slot| *slot.borrow_mut() = None);
}

/// Whether a sink is installed: [`shard!`](crate::shard!) evaluates its
/// arguments only then.
#[must_use]
pub fn is_active() -> bool {
    SINK.with(|slot| slot.borrow().is_some())
}

/// Declare `name` at `site`. Use [`shard!`](crate::shard!), which names the
/// site and skips the call outside a simulation.
pub fn declare_at(
    name: impl Into<String>,
    current: Option<Replication>,
    next: Option<Replication>,
    site: &'static str,
) {
    SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink.declare(Declaration {
                name: name.into(),
                current,
                next,
                site,
            });
        }
    });
}

/// Say that the calling process's copy of `shard` opened intact: a store
/// verified it at open and kept it. A simulator counts a copy whose last
/// batch a crash tore as lost until this call, because a format may refuse
/// such a copy whole. Call it only after the store's own check passed; a
/// copy the store refuses stays lost. Inert outside a simulation.
pub fn intact(shard: &str) {
    SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink.intact(shard);
        }
    });
}

/// Say that `replica`'s copy of `shard` has caught up: a replica wiped
/// earlier is intact again once a declaration made after the wipe lists it
/// and this call follows. Inert outside a simulation.
pub fn caught_up(shard: &str, replica: &Replica) {
    SINK.with(|slot| {
        if let Some(sink) = slot.borrow().as_ref() {
            sink.caught_up(shard, replica);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn replicas(names: &[&str]) -> Vec<Replica> {
        names.iter().map(|name| Replica::new(*name)).collect()
    }

    #[test]
    fn at_least_counts_distinct_admitted_replicas() {
        let need = Need::majority(replicas(&["a", "b", "c"]));
        assert!(need.holds(&|r: &Replica| r.0 != "a"));
        assert!(!need.holds(&|r: &Replica| r.0 == "a"));
        let doubled = Need::AtLeast(2, replicas(&["a", "a"]));
        assert!(!doubled.holds(&|_| true), "a duplicate counts once");
    }

    #[test]
    fn all_and_any_compose() {
        let p1 = Need::majority(replicas(&["a", "b", "c"]));
        let p2 = Need::majority(replicas(&["d", "e", "f"]));
        let election = Need::All(vec![p1.clone(), p2.clone()]);
        let write = Need::Any(vec![p1, p2]);
        let up = |r: &Replica| ["a", "b", "d"].contains(&r.0.as_str());
        assert!(!election.holds(&up));
        assert!(write.holds(&up));
        assert_eq!(election.replicas().len(), 6);
    }

    #[test]
    fn minimal_sets_of_a_majority_are_its_pairs() {
        let need = Need::majority(replicas(&["a", "b", "c"]));
        let sets = need.minimal_sets(&|_| true);
        assert_eq!(
            sets,
            vec![
                replicas(&["a", "b"]),
                replicas(&["a", "c"]),
                replicas(&["b", "c"])
            ]
        );
        let without_a = need.minimal_sets(&|r: &Replica| r.0 != "a");
        assert_eq!(without_a, vec![replicas(&["b", "c"])]);
        assert!(need.minimal_sets(&|r: &Replica| r.0 == "a").is_empty());
    }

    #[test]
    fn minimal_sets_drop_supersets() {
        let need = Need::Any(vec![
            Need::AtLeast(1, replicas(&["a"])),
            Need::AtLeast(2, replicas(&["a", "b"])),
        ]);
        assert_eq!(need.minimal_sets(&|_| true), vec![replicas(&["a"])]);
        let both = Need::All(vec![
            Need::majority(replicas(&["a", "b", "c"])),
            Need::majority(replicas(&["c", "d", "e"])),
        ]);
        let sets = both.minimal_sets(&|_| true);
        assert!(sets.contains(&replicas(&["a", "c", "d"])));
        assert!(sets.iter().all(|set| both.holds(&|r| set.contains(r))));
    }

    #[test]
    fn tolerates_builds_count_needs() {
        let r = Replication::tolerates(replicas(&["a", "b", "c"]), 1, 2);
        assert_eq!(r.progress, Need::AtLeast(2, replicas(&["a", "b", "c"])));
        assert_eq!(r.durable, Need::AtLeast(1, replicas(&["a", "b", "c"])));
        let clamped = Replication::tolerates(replicas(&["a"]), 5, 5);
        assert_eq!(clamped.progress, Need::AtLeast(0, replicas(&["a"])));
        assert_eq!(
            r.to_string(),
            "progress: at least 2 of [a, b, c]; durable: at least 1 of [a, b, c]"
        );
    }

    struct Recording(Rc<RefCell<Vec<Declaration>>>);

    impl Sink for Recording {
        fn declare(&self, declaration: Declaration) {
            self.0.borrow_mut().push(declaration);
        }
        fn caught_up(&self, _shard: &str, _replica: &Replica) {}
        fn intact(&self, _shard: &str) {}
    }

    #[test]
    fn without_a_sink_the_arguments_are_not_evaluated() {
        clear_sink();
        let evaluated = std::cell::Cell::new(false);
        let current = || {
            evaluated.set(true);
            Replication::tolerates(replicas(&["a"]), 0, 0)
        };
        crate::shard!("idle", current = current());
        assert!(!evaluated.get());
    }

    #[test]
    fn a_sink_receives_every_form() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        set_sink(Box::new(Recording(Rc::clone(&seen))));
        let r1 = Replication::tolerates(replicas(&["a", "b", "c"]), 1, 1);
        let r2 = Replication::tolerates(replicas(&["b", "c", "d"]), 1, 1);
        crate::shard!("s", current = r1.clone());
        crate::shard!("s", current = r1.clone(), next = r2.clone());
        crate::shard!(String::from("s"), current = r2.clone(),);
        crate::shard!("s");
        clear_sink();
        let seen = seen.borrow();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[1].current, Some(r1));
        assert_eq!(seen[1].next, Some(r2.clone()));
        assert_eq!(seen[2].current, Some(r2));
        assert_eq!(seen[3].current, None);
        assert!(seen[0].site.contains("shard.rs"));
        assert_ne!(seen[0].site, seen[1].site, "the call site names a move");
    }
}
