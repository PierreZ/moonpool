//! Extension points: reply classification, opt-in comparison copies,
//! attempt observation (`FoundationDB`'s `LoadBalanceRequestHooks`, with
//! no storage or TSS semantics), and selection ([`Selector`], with the
//! queue-model default and a busy-score alternative).
//!
//! Hooks advise; the balancer decides. A classifier cannot turn an error
//! into a reply, a comparison request cannot bypass
//! [`Duplicates`](super::Duplicates) or the hedge budget, and a selector
//! cannot pick an alternative the failure monitor knows is gone, nor hide
//! every reachable one.

use std::time::Duration;

use super::locality::Distance;
use super::model::{BAD_PENALTY, Feedback};
use crate::endpoint::Endpoint;
use crate::error::{Execution, RpcError};
use crate::protocol::RpcMethod;

/// What an application makes of a reply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Use it, with the server's load feedback.
    Accept(Feedback),
    /// The server declined without effect because it is temporarily
    /// behind (the application's notion, for example a replica that has
    /// not caught up yet): exclude it for a growing backoff and try
    /// another alternative.
    Behind,
    /// The server declined without effect for load: exclude it like
    /// [`Behind`](Self::Behind), record its feedback (a penalty), and try
    /// another alternative. The same as the transport's
    /// [`ErrorReason::Overloaded`](crate::ErrorReason::Overloaded) refusal.
    Overloaded(Feedback),
}

/// Why an attempt was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttemptKind {
    /// The call's first attempt.
    First,
    /// A sequential fail-over after an earlier attempt ended.
    Retry,
    /// A concurrent copy of a slow attempt.
    Hedge,
    /// A concurrent copy requested by [`BalanceHooks::wants_comparison`].
    /// It completes the caller only when no primary attempt can: once
    /// every primary attempt failed or was declined, an accepted
    /// comparison reply is the winner (uncompared).
    Comparison,
}

/// How an attempt ended, as seen by [`BalanceHooks::observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEnd {
    /// A reply arrived (whatever the classifier made of it).
    Replied,
    /// An error; it says what it proves about execution.
    Failed(RpcError),
    /// Dropped before an outcome arrived (lagging bound or timeout, or the
    /// model went away): may have executed.
    Dropped,
}

impl AttemptEnd {
    /// What the end proves about execution.
    #[must_use]
    pub fn execution(&self) -> Execution {
        match self {
            Self::Replied => Execution::Executed,
            Self::Failed(error) => error.execution(),
            Self::Dropped => Execution::MaybeExecuted,
        }
    }
}

/// An attempt's life, reported to [`BalanceHooks::observe`]. Every
/// `Started` is followed by exactly one `Ended`, possibly after the call
/// returned (a late loser).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEvent {
    /// An attempt was started (and reserved in the model).
    Started {
        /// Index of the alternative in the call's set.
        alternative: usize,
        /// The endpoint incarnation it went to.
        endpoint: Endpoint,
        /// Why.
        kind: AttemptKind,
    },
    /// It ended (and released its reservation).
    Ended {
        /// Index of the alternative in the call's set.
        alternative: usize,
        /// The endpoint incarnation it went to.
        endpoint: Endpoint,
        /// Why it was started.
        kind: AttemptKind,
        /// How it ended.
        end: AttemptEnd,
        /// Whether its call had already finished (or was dropped).
        late: bool,
    },
}

/// Application hooks of a balanced client. Every method has a neutral
/// default; implement only what you need.
pub trait BalanceHooks<M: RpcMethod>: Send + Sync + 'static {
    /// Classify a reply (also a late loser's, for the model). Default:
    /// accept with no feedback.
    fn classify(&self, reply: &M::Reply) -> Verdict {
        let _ = reply;
        Verdict::Accept(Feedback::default())
    }

    /// Whether this request should also go to a second alternative so its
    /// reply can be compared with the winner's. Honoured only when the
    /// call's policy permits duplicates, a copy remains in the policy's
    /// `max_copies` and the model's hedge budget grants a unit.
    fn wants_comparison(&self, request: &M::Request) -> bool {
        let _ = request;
        false
    }

    /// Compare the winning reply with the comparison copy's outcome
    /// (`None`: it did not arrive within
    /// [`DuplicatePolicy::compare_within`](super::DuplicatePolicy::compare_within)).
    /// A call has at most one comparison copy, started next to its first
    /// attempt; the winner may be that attempt or a later retry or hedge.
    /// All of them carry the same request, so the comparison is between
    /// two answers to one request whichever round won.
    /// An error fails the call with
    /// [`BalanceFailure::Comparison`](super::BalanceFailure::Comparison).
    ///
    /// # Errors
    ///
    /// A description of the disagreement.
    fn compare(
        &self,
        winner: &M::Reply,
        copy: Option<&Result<M::Reply, RpcError>>,
    ) -> Result<(), String> {
        let _ = (winner, copy);
        Ok(())
    }

    /// Observe attempt starts and ends (tracing, ledgers). Must not block.
    fn observe(&self, event: &AttemptEvent) {
        let _ = event;
    }
}

/// The neutral hooks: accept every reply, never compare.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultHooks;

impl<M: RpcMethod> BalanceHooks<M> for DefaultHooks {}

/// One alternative as a [`Selector`] sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// Index of the alternative in the set.
    pub index: usize,
    /// Its distance from the caller.
    pub distance: Distance,
    /// Its address is not known to be failed. Unavailable candidates are
    /// shown so a selector can count them as bad. They are tried only as a
    /// probe, once per call, when no alternative looks reachable after the
    /// bounded wait (a failed address only changes on a new connection).
    pub available: bool,
    /// Temporarily excluded (behind or overloaded). Still tried when
    /// nothing better is left.
    pub excluded: bool,
    /// The server's latest penalty.
    pub penalty: f64,
    /// Weighted outstanding work from this client.
    pub outstanding: f64,
    /// Measured latency.
    pub latency: Duration,
    /// The latest busy score and its age.
    pub busy: Option<(u32, Duration)>,
}

impl Candidate {
    /// Unavailable, excluded, or asking to be avoided.
    #[must_use]
    pub fn is_bad(&self) -> bool {
        !self.available || self.excluded || self.penalty > BAD_PENALTY
    }
}

/// Orders alternatives for a call.
///
/// `candidates` lists every alternative not known to be permanently gone;
/// `draw(n)` is a uniform provider-RNG draw in `0..n` (`n > 0`). Return
/// alternative indices, best first. The balancer tries them in that
/// order, skips indices that are unknown, unavailable (except in the
/// probe, where every candidate is usable) or already in flight for the
/// call, and appends any usable candidate the selector left out, so no
/// selector can starve a reachable alternative.
pub trait Selector: Send + Sync + 'static {
    /// The preference order.
    fn order(&self, candidates: &[Candidate], draw: &mut dyn FnMut(u64) -> u64) -> Vec<usize>;
}

/// The default: `FoundationDB`'s queue-model choice. The nearest
/// alternatives are ranked by weighted outstanding work (penalties
/// included), bad ones last; farther alternatives join on equal terms
/// once more than `max_bad_options` of the nearest are bad. Ties break at
/// random.
#[derive(Debug, Clone, Copy)]
pub struct QueueSelector {
    /// `LOAD_BALANCE_MAX_BAD_OPTIONS` (1).
    pub max_bad_options: usize,
}

impl Default for QueueSelector {
    fn default() -> Self {
        Self { max_bad_options: 1 }
    }
}

fn tie_breakers(count: usize, draw: &mut dyn FnMut(u64) -> u64) -> Vec<u64> {
    (0..count).map(|_| draw(u64::from(u32::MAX))).collect()
}

impl Selector for QueueSelector {
    fn order(&self, candidates: &[Candidate], draw: &mut dyn FnMut(u64) -> u64) -> Vec<usize> {
        let keys = tie_breakers(candidates.len(), draw);
        let Some(nearest) = candidates.iter().map(|c| c.distance).min() else {
            return Vec::new();
        };
        let best = candidates.iter().filter(|c| c.distance == nearest).count();
        let bad = candidates
            .iter()
            .filter(|c| c.distance == nearest && c.is_bad())
            .count();
        // FDB: stay local while fewer than min(countBest, MAX_BAD + 1) of
        // the local alternatives are bad.
        let spill = bad >= best.min(self.max_bad_options + 1);
        // Without a spill the nearest tier comes first and farther ones
        // follow by distance; with one, distance no longer matters.
        let tier = |c: &Candidate| {
            if spill {
                Distance::SameMachine
            } else {
                c.distance
            }
        };
        let avoid = |c: &Candidate| u8::from(!c.available || c.excluded);
        let mut ranked: Vec<(usize, &Candidate)> = candidates.iter().enumerate().collect();
        ranked.sort_by(|(i, a), (j, b)| {
            tier(a)
                .cmp(&tier(b))
                .then(avoid(a).cmp(&avoid(b)))
                .then(a.outstanding.total_cmp(&b.outstanding))
                .then(keys[*i].cmp(&keys[*j]))
        });
        ranked.into_iter().map(|(_, c)| c.index).collect()
    }
}

/// Basic busy-score selection (`FoundationDB`'s `basicLoadBalance` with a
/// `ModelInterface`): a random order weighted towards alternatives that
/// reported a lower busy score ([`Feedback::busy`]). Scores older than
/// `max_age`, or never reported, count as idle. Locality is ignored, as
/// in `basicLoadBalance`.
#[derive(Debug, Clone, Copy)]
pub struct BusyScoreSelector {
    /// How long a reported score stays meaningful.
    pub max_age: Duration,
}

impl Default for BusyScoreSelector {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(10),
        }
    }
}

impl Selector for BusyScoreSelector {
    fn order(&self, candidates: &[Candidate], draw: &mut dyn FnMut(u64) -> u64) -> Vec<usize> {
        let mut pool: Vec<(usize, u64)> = candidates
            .iter()
            .filter(|c| c.available && !c.excluded)
            .map(|c| {
                let busy = match c.busy {
                    Some((score, age)) if age <= self.max_age => u64::from(score),
                    _ => 0,
                };
                (c.index, 1_000_000 / (1 + busy))
            })
            .collect();
        let mut order = Vec::with_capacity(candidates.len());
        while !pool.is_empty() {
            let total: u64 = pool.iter().map(|(_, weight)| weight.max(&1)).sum();
            let mut ticket = draw(total.max(1));
            let mut chosen = pool.len() - 1;
            for (position, (_, weight)) in pool.iter().enumerate() {
                let weight = (*weight).max(1);
                if ticket < weight {
                    chosen = position;
                    break;
                }
                ticket -= weight;
            }
            order.push(pool.remove(chosen).0);
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BusyScoreSelector, Candidate, QueueSelector, Selector};
    use crate::balance::locality::Distance;

    fn candidate(index: usize, distance: Distance, outstanding: f64) -> Candidate {
        Candidate {
            index,
            distance,
            available: true,
            excluded: false,
            penalty: 1.0,
            outstanding,
            latency: Duration::from_millis(1),
            busy: None,
        }
    }

    fn fixed_draw() -> impl FnMut(u64) -> u64 {
        let mut next = 0u64;
        move |n| {
            next = next
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (next >> 11) % n
        }
    }

    #[test]
    fn nearest_first_then_by_outstanding_work() {
        let selector = QueueSelector::default();
        let candidates = [
            candidate(0, Distance::Distant, 0.0),
            candidate(1, Distance::SameDatacenter, 3.0),
            candidate(2, Distance::SameMachine, 5.0),
            candidate(3, Distance::SameMachine, 1.0),
        ];
        let order = selector.order(&candidates, &mut fixed_draw());
        assert_eq!(order, vec![3, 2, 1, 0]);
    }

    #[test]
    fn too_many_bad_local_alternatives_spill_to_remote_ones() {
        let selector = QueueSelector::default();
        let mut local = candidate(0, Distance::SameDatacenter, 0.0);
        local.excluded = true;
        let mut penalized = candidate(1, Distance::SameDatacenter, 0.0);
        penalized.penalty = 2.0;
        let remote = candidate(2, Distance::Distant, 0.5);
        let order = selector.order(&[local, penalized, remote], &mut fixed_draw());
        // Both local ones are bad (>= min(2, 2)): the remote competes on
        // equal terms and wins over the excluded one; the penalized local
        // one still has less work.
        assert_eq!(order, vec![1, 2, 0]);
        // One bad local alternative out of two stays local.
        let fine = candidate(1, Distance::SameDatacenter, 4.0);
        let order = selector.order(&[local, fine, remote], &mut fixed_draw());
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn excluded_and_unavailable_sort_last_but_stay_listed() {
        let selector = QueueSelector::default();
        let mut excluded = candidate(0, Distance::Distant, 0.0);
        excluded.excluded = true;
        let mut down = candidate(1, Distance::Distant, 0.0);
        down.available = false;
        let healthy = candidate(2, Distance::Distant, 9.0);
        let order = selector.order(&[excluded, down, healthy], &mut fixed_draw());
        assert_eq!(order[0], 2);
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn ties_break_at_random_and_empty_is_empty() {
        let selector = QueueSelector::default();
        assert!(selector.order(&[], &mut fixed_draw()).is_empty());
        let candidates: Vec<_> = (0..4)
            .map(|i| candidate(i, Distance::Distant, 0.0))
            .collect();
        let mut firsts = std::collections::BTreeSet::new();
        let mut draw = fixed_draw();
        for _ in 0..64 {
            firsts.insert(selector.order(&candidates, &mut draw)[0]);
        }
        assert_eq!(firsts.len(), 4, "every tied alternative gets picked first");
    }

    #[test]
    fn busy_scores_bias_the_order() {
        let selector = BusyScoreSelector::default();
        let mut busy = candidate(0, Distance::Distant, 0.0);
        busy.busy = Some((999, Duration::from_secs(1)));
        let mut idle = candidate(1, Distance::Distant, 0.0);
        idle.busy = Some((0, Duration::from_secs(1)));
        let mut stale = candidate(2, Distance::Distant, 0.0);
        stale.busy = Some((999, Duration::from_mins(1)));
        let mut draw = fixed_draw();
        let mut first = [0u32; 3];
        for _ in 0..300 {
            let order = selector.order(&[busy, idle, stale], &mut draw);
            assert_eq!(order.len(), 3);
            first[order[0]] += 1;
        }
        assert!(first[0] < 10, "{first:?}");
        assert!(first[1] > 100 && first[2] > 100, "{first:?}");
    }
}
