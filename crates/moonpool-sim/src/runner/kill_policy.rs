//! The harness's kill policy: the simulator asks it before each destructive
//! fault, as `FoundationDB`'s `canKillProcesses` does.
//!
//! The simulator knows which processes are dead and which disks were wiped,
//! but not what the system under test promises to survive. A harness that
//! knows it publishes a [`KillPolicy`] in the run's state. Every kill and
//! every wipe the [`FaultContext`](crate::FaultContext) runs (attrition, a
//! reboot hint, an outage, a scripted injector's `reboot`, `crash` and
//! `crash_for`) asks it first, before the shard gate:
//!
//! - [`KillVerdict::Allow`]: the fault runs as drawn.
//! - [`KillVerdict::KeepDisk`]: a wipe runs as a crash, and the disk stays.
//!   `FoundationDB` turns a `RebootAndDelete` that would leave too few
//!   copies into a `Reboot` (`sim2.actor.cpp`, `canKillProcesses`, release
//!   7.3.43). On a kill that is no wipe, the same as `Allow`.
//! - [`KillVerdict::Restart`]: the process is not crashed. A reboot or a
//!   hold-down restarts it in place, as the shard gate's clean restart does
//!   (no crash physics, its disk kept), and a timed crash (an outage,
//!   [`crash_for`](crate::FaultContext::crash_for)) spares it. `FoundationDB`
//!   turns a kill that would leave too many processes dead into a `Reboot`.
//!
//! The policy must not draw randomness: its answer must not change the draw
//! schedule. It is asked at the decision, with the processes dead and wiped
//! at that instant; the victim is not among them.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;

use crate::RebootKind;

/// The run's state key a [`KillPolicy`] is published under.
pub const KILL_POLICY_KEY: &str = "moonpool:kill-policy";

/// One destructive fault the simulator is about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillAsk {
    /// The process the fault takes down.
    pub victim: IpAddr,
    /// The fault: [`RebootKind::CrashAndWipe`] loses the disk, every other
    /// kind keeps it.
    pub kind: RebootKind,
    /// Whether the process stays down until an explicit restart
    /// ([`FaultContext::crash`](crate::FaultContext::crash)), rather than
    /// coming back after a delay the simulator schedules.
    pub held: bool,
    /// The processes dead now, the victim excluded.
    pub dead: BTreeSet<IpAddr>,
    /// The processes wiped and not yet healed now
    /// ([`WipeLedger::unhealed`](crate::WipeLedger::unhealed)), the victim
    /// excluded.
    pub wiped: BTreeSet<IpAddr>,
}

impl KillAsk {
    /// Whether the fault loses the victim's disk.
    #[must_use]
    pub fn wipes(&self) -> bool {
        self.kind == RebootKind::CrashAndWipe
    }
}

/// A [`KillPolicy`]'s answer (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillVerdict {
    /// The fault runs as drawn.
    Allow,
    /// A wipe runs as a crash: the disk stays.
    KeepDisk,
    /// The victim is not crashed: it restarts in place, or a timed crash
    /// spares it.
    Restart,
}

/// What a [`KillPolicy`] answers.
type Judge = dyn Fn(&KillAsk) -> KillVerdict + Send + Sync;

/// A harness's policy over the destructive faults (see the module doc).
#[derive(Clone)]
pub struct KillPolicy(Arc<Judge>);

impl KillPolicy {
    /// A policy answering `judge(ask)`.
    pub fn new(judge: impl Fn(&KillAsk) -> KillVerdict + Send + Sync + 'static) -> Self {
        Self(Arc::new(judge))
    }

    /// Install this policy for the run that owns `state`, replacing any
    /// earlier one.
    pub fn publish(self, state: &crate::StateHandle) {
        state.publish(KILL_POLICY_KEY, self);
    }

    /// The policy of the run that owns `state`, if one is published.
    pub(crate) fn of(state: &crate::StateHandle) -> Option<Self> {
        state.get::<Self>(KILL_POLICY_KEY)
    }

    /// The policy's answer to `ask`.
    pub(crate) fn judge(&self, ask: &KillAsk) -> KillVerdict {
        (self.0)(ask)
    }
}

impl std::fmt::Debug for KillPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KillPolicy")
    }
}
