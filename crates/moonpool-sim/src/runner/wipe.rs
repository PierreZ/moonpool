//! Rolling wipes: a bound on how many wiped processes of a victim pool may
//! be unhealed at once.
//!
//! [`Attrition::max_dead`](super::process::Attrition::max_dead) bounds the
//! processes *dead* at the same time. A
//! [`CrashAndWipe`](super::process::RebootKind::CrashAndWipe) process
//! restarts quickly, though, and comes back empty: it is alive long before
//! the system under test has rebuilt what it lost. Two wipes a few seconds
//! apart each pass `max_dead: 1`, yet together they can erase two of three
//! replicas of the same state. A [`WipeBudget`] closes that gap: a wiped
//! process stays **unhealed** from the wipe decision until it is healed,
//! and while the pool holds `max_unhealed` unhealed processes, a drawn wipe
//! becomes a plain crash. Wipes then roll through the pool one healed
//! process at a time, as an operator replacing disks would.
//!
//! # Healing
//!
//! [`WipeHeal::Signal`] lets the system under test say when a wiped
//! process has caught up: the harness calls [`WipeLedger::heal`] with the
//! process's IP, from a process, a workload or an invariant-side hook, on
//! the run's [`StateHandle`].
//! [`WipeHeal::After`] heals a process a fixed simulated time after its
//! restart, with no cooperation from the code under test.
//!
//! # Determinism
//!
//! The budget draws nothing. A downgrade is decided *after* the reboot kind
//! is drawn, from the ledger alone, so a regime with a budget makes exactly
//! the draws the same regime without one makes, and a regime without one
//! (`wipe_budget: None`) never reads the ledger at all.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::chaos::state_handle::StateHandle;

/// The run's [`StateHandle`] key the [`WipeLedger`] is
/// published under. The runner publishes a fresh ledger for every
/// iteration; do not publish another value under this key.
pub const WIPE_LEDGER_KEY: &str = "moonpool:wipe-ledger";

/// How a wiped process becomes healed again under a [`WipeBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WipeHeal {
    /// Healed only when the harness says so, with [`WipeLedger::heal`]:
    /// the system under test knows when a replaced node has caught up,
    /// the simulator does not. A process never signaled stays unhealed for
    /// the rest of the run, so later wipes in its pool all become crashes.
    Signal,
    /// Healed this long (simulated time) after the wiped process restarts.
    /// [`WipeLedger::heal`] heals it earlier too.
    After(Duration),
}

/// A bound on unhealed wiped processes in one attrition victim pool.
///
/// See the [module documentation](self) and
/// [`Attrition::wipe_budget`](super::process::Attrition::wipe_budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WipeBudget {
    /// The most wiped processes of the victim pool that may be unhealed at
    /// once. A drawn wipe that would pass it is downgraded to a crash; a
    /// group (domain) wipe counts every process of the group. A victim that
    /// is itself unhealed is not counted twice: wiping it again loses
    /// nothing more. `0` turns every wipe of the regime into a crash.
    pub max_unhealed: usize,
    /// How a wiped process heals.
    pub heal: WipeHeal,
}

/// One wiped process not yet healed by a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wiped {
    /// When the wiped process restarted, once it has.
    restarted_at: Option<Duration>,
}

impl Wiped {
    /// Whether this wipe is still unhealed under `heal` at `now`.
    fn unhealed(self, heal: WipeHeal, now: Duration) -> bool {
        match heal {
            WipeHeal::Signal => true,
            WipeHeal::After(bound) => self
                .restarted_at
                .is_none_or(|restarted| now < restarted.saturating_add(bound)),
        }
    }
}

/// The run's record of wiped processes not yet signaled healed, keyed by
/// process IP.
///
/// The runner publishes one per iteration under [`WIPE_LEDGER_KEY`]. Every
/// wipe is recorded, whoever decided it: an attrition regime, a scripted
/// [`FaultContext::reboot`](crate::FaultContext::reboot) with
/// [`RebootKind::CrashAndWipe`](crate::RebootKind::CrashAndWipe), or a
/// process [crashing itself](crate::SelfCrash::crash). Only a regime with a
/// [`WipeBudget`] reads it, and only to downgrade its own drawn wipes:
/// scripted wipes are never refused, but they do count against a budgeted
/// pool they land in.
#[derive(Debug, Clone, Default)]
pub struct WipeLedger(Arc<Mutex<BTreeMap<IpAddr, Wiped>>>);

impl WipeLedger {
    /// Declare the wiped process at `ip` healed, in the run that owns
    /// `state`: it no longer counts against any [`WipeBudget`]. Returns
    /// whether it was unhealed. A process that was not wiped, an
    /// unparsable IP, or a state without a ledger is a no-op returning
    /// `false`, so calling it on every catch-up is safe. Draws nothing.
    ///
    /// # Panics
    ///
    /// Panics if the ledger's lock is poisoned (a prior task panicked).
    pub fn heal(state: &StateHandle, ip: &str) -> bool {
        let Ok(ip) = ip.parse::<IpAddr>() else {
            return false;
        };
        let Some(ledger) = state.get::<WipeLedger>(WIPE_LEDGER_KEY) else {
            return false;
        };
        let healed = ledger.lock().remove(&ip).is_some();
        if healed {
            crate::assert_reachable!("wipe budget: a wiped process signaled healed");
            tracing::info!(%ip, "wiped process healed");
        }
        healed
    }

    /// The processes wiped in the run that owns `state` and not yet
    /// declared healed by [`heal`](Self::heal), ascending. A
    /// [`WipeHeal::After`] bound heals without removing a process from this
    /// list. Empty when `state` holds no ledger.
    ///
    /// # Panics
    ///
    /// Panics if the ledger's lock is poisoned (a prior task panicked).
    #[must_use]
    pub fn unhealed(state: &StateHandle) -> Vec<IpAddr> {
        state
            .get::<WipeLedger>(WIPE_LEDGER_KEY)
            .map(|ledger| ledger.lock().keys().copied().collect())
            .unwrap_or_default()
    }

    /// Publish a fresh ledger into `state`.
    pub(crate) fn install(state: &StateHandle) {
        state.publish(WIPE_LEDGER_KEY, WipeLedger::default());
    }

    /// The ledger of the run that owns `state`, if one is published.
    pub(crate) fn of(state: &StateHandle) -> Option<WipeLedger> {
        state.get::<WipeLedger>(WIPE_LEDGER_KEY)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<IpAddr, Wiped>> {
        self.0.lock().expect("Mutex poisoned: prior task panicked")
    }

    /// Record a wipe decided now: `ip` is unhealed and not yet restarted,
    /// whatever an earlier wipe of it recorded.
    pub(crate) fn record_wipe(&self, ip: IpAddr) {
        self.lock().insert(ip, Wiped { restarted_at: None });
    }

    /// Record a wipe at its kill: a wipe whose decision did not record it
    /// (a process [crashing itself](crate::SelfCrash::crash)) becomes
    /// unhealed; one its decision recorded keeps that record.
    pub(crate) fn note_wipe_kill(&self, ip: IpAddr) {
        self.lock()
            .entry(ip)
            .or_insert(Wiped { restarted_at: None });
    }

    /// The process at `ip` restarted at `now`: an unhealed wipe of it
    /// starts its [`WipeHeal::After`] clock.
    pub(crate) fn note_restart(&self, ip: IpAddr, now: Duration) {
        if let Some(wiped) = self.lock().get_mut(&ip)
            && wiped.restarted_at.is_none()
        {
            wiped.restarted_at = Some(now);
        }
    }

    /// How many of `ips` are unhealed under `heal` at `now`.
    pub(crate) fn unhealed_among(&self, ips: &[&String], heal: WipeHeal, now: Duration) -> usize {
        let ledger = self.lock();
        ips.iter()
            .filter_map(|ip| ip.parse::<IpAddr>().ok())
            .filter(|ip| ledger.get(ip).is_some_and(|w| w.unhealed(heal, now)))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 1, last])
    }

    #[test]
    fn heal_removes_a_recorded_wipe_once() {
        let state = StateHandle::new();
        WipeLedger::install(&state);
        let ledger = WipeLedger::of(&state).expect("installed");
        ledger.record_wipe(ip(1));
        assert_eq!(WipeLedger::unhealed(&state), vec![ip(1)]);
        assert!(WipeLedger::heal(&state, "10.0.1.1"));
        assert!(!WipeLedger::heal(&state, "10.0.1.1"));
        assert!(WipeLedger::unhealed(&state).is_empty());
    }

    #[test]
    fn heal_without_a_ledger_is_a_no_op() {
        let state = StateHandle::new();
        assert!(!WipeLedger::heal(&state, "10.0.1.1"));
        assert!(!WipeLedger::heal(&state, "not an ip"));
        assert!(WipeLedger::unhealed(&state).is_empty());
    }

    #[test]
    fn after_heals_from_the_restart_not_the_wipe() {
        let ledger = WipeLedger::default();
        let one = "10.0.1.1".to_string();
        let pool = [&one];
        let heal = WipeHeal::After(Duration::from_secs(5));
        ledger.record_wipe(ip(1));
        // Not restarted yet: unhealed however long it has been down.
        assert_eq!(
            ledger.unhealed_among(&pool, heal, Duration::from_secs(60)),
            1
        );
        ledger.note_restart(ip(1), Duration::from_secs(60));
        assert_eq!(
            ledger.unhealed_among(&pool, heal, Duration::from_secs(64)),
            1
        );
        assert_eq!(
            ledger.unhealed_among(&pool, heal, Duration::from_secs(65)),
            0
        );
        // A signal-healed regime still counts it.
        assert_eq!(
            ledger.unhealed_among(&pool, WipeHeal::Signal, Duration::from_secs(65)),
            1
        );
    }

    #[test]
    fn a_wipe_kill_records_an_unrecorded_wipe_and_keeps_a_recorded_one() {
        let ledger = WipeLedger::default();
        let one = "10.0.1.1".to_string();
        let heal = WipeHeal::After(Duration::from_secs(1));
        ledger.note_wipe_kill(ip(1));
        assert_eq!(ledger.unhealed_among(&[&one], heal, Duration::ZERO), 1);
        ledger.note_restart(ip(1), Duration::ZERO);
        // A second kill notice must not reset the restart clock.
        ledger.note_wipe_kill(ip(1));
        assert_eq!(
            ledger.unhealed_among(&[&one], heal, Duration::from_secs(1)),
            0
        );
    }
}
