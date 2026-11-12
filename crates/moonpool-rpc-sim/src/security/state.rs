//! The independent ledger: what every caller issued (written before the
//! request leaves) and what every handler received (written when user code
//! gets it), judged without reading the transport.
//!
//! **The oracle.** A request may reach a handler only if its credential
//! could have been accepted at some moment between its sending and its
//! receipt: the scripted UTC only moves forward, and key sets only change
//! by replacement, so verification happened at a UTC in
//! `[utc_sent, utc_received]` under a key-set generation in
//! `[generation_sent, generation_received]`. A properly signed token is
//! acceptable there iff its `[nbf, exp)` window meets the UTC interval and
//! its key was published in one of those generations. Anything else
//! (forged, unknown key, wrong audience or issuer, a symmetric algorithm,
//! garbage) is never acceptable; an anonymous request reaches only public
//! endpoints; a version 1 session carries no credential at all.
//!
//! **Fail closed with no UTC.** The ledger also stamps the clock epoch of
//! [`Trust`] (odd while the servers have no UTC) when a request is sent,
//! when a handler receives it and when its caller sees the outcome. A
//! signed token can run only if the servers had the time at some moment in
//! that interval, and a `ClockUnavailable` refusal is honest only if they
//! lacked it at some moment in that interval.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_rpc::security::CredentialError;
use moonpool_sim::StateHandle;

use super::trust::{Kind, Minted, Trust};
use crate::lock;

const LEDGER_KEY: &str = "rpc.security.ledger";
/// The server's latest references, as [`ServerRefs`].
pub const SERVER_REFS_KEY: &str = "rpc.security.server.refs";
/// The legacy server's latest references, as [`LegacyRefs`].
pub const LEGACY_REFS_KEY: &str = "rpc.security.legacy.refs";
/// How many times the server booted.
pub const SERVER_BOOTS_KEY: &str = "rpc.security.server.boots";
/// How many times the legacy server booted.
pub const LEGACY_BOOTS_KEY: &str = "rpc.security.legacy.boots";
/// Set once the fault script stopped (and restored the clock).
pub const SCRIPT_DONE_KEY: &str = "rpc.security.script.done";
/// The boot of the server whose graceful shutdown is draining, as `u64`.
pub const DRAINING_KEY: &str = "rpc.security.server.draining";
/// The workload's IP, for scripted cuts.
pub const WORKLOAD_IP_KEY: &str = "rpc.security.workload.ip";
/// How many cuts between the workload and the server were asked for.
pub const CUT_REQUESTS_KEY: &str = "rpc.security.cuts";

/// Serialised references a server boot published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRefs {
    /// The boot that registered them.
    pub boot: u64,
    /// [`PrivateEcho`](super::messages::PrivateEcho).
    pub private: Vec<u8>,
    /// [`PublicEcho`](super::messages::PublicEcho).
    pub public: Vec<u8>,
    /// [`PrivateScan`](super::messages::PrivateScan).
    pub scan: Vec<u8>,
    /// [`PrivateNote`](super::messages::PrivateNote).
    pub note: Vec<u8>,
}

/// Serialised references the legacy server published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyRefs {
    /// Its public echo.
    pub public: Vec<u8>,
    /// Its private echo.
    pub private: Vec<u8>,
}

/// What an outcome proves, as the ledger judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// A reply (or stream items) came back.
    Replied,
    /// Refused before any handler.
    NotAdmitted,
    /// Unknown: a disconnect or timeout after sending, or a one-way send.
    Maybe,
    /// The handler ran but its outcome was unusable.
    Executed,
}

/// Which endpoint a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// The server's private unary endpoint.
    Private,
    /// The server's public unary endpoint.
    Public,
    /// The server's private streaming endpoint.
    Scan,
    /// The server's private endpoint, called one-way.
    Note,
    /// The version 1 legacy server's public endpoint.
    LegacyPublic,
    /// The version 1 legacy server's private endpoint.
    LegacyPrivate,
}

/// Who sent it, over what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    /// The workload's runtime (protocol versions 1 and 2).
    Remote,
    /// A caller inside the server's own runtime.
    Local,
    /// The workload's version 1 only runtime.
    Version1,
}

/// One request as its caller issued it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The credential.
    pub minted: Minted,
    /// The subject its credential names.
    pub subject: String,
    /// Its endpoint.
    pub target: Target,
    /// Its sender.
    pub route: Route,
    /// The script's UTC when it was sent.
    pub utc_sent: u64,
    /// The key-set generation when it was sent.
    pub generation_sent: u64,
    /// The [`Trust::clock_epoch`] when it was sent.
    pub clock_sent: u64,
}

/// One handler receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The endpoint that received it.
    pub target: Target,
    /// The principal the server verified (`None`: anonymous).
    pub subject: Option<String>,
    /// The script's UTC at receipt.
    pub utc: u64,
    /// The key-set generation at receipt.
    pub generation: u64,
    /// The [`Trust::clock_epoch`] at receipt.
    pub clock: u64,
}

impl Receipt {
    /// A receipt by `target` now, of a request whose session proved
    /// `subject`.
    #[must_use]
    pub fn now(target: Target, subject: Option<String>, trust: &Trust) -> Self {
        Self {
            target,
            subject,
            utc: trust.utc(),
            generation: trust.generation(),
            clock: trust.clock_epoch(),
        }
    }
}

/// When a credential was minted or a request was seen: the script's UTC,
/// the key-set generation and the clock epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// The script's UTC.
    pub utc: u64,
    /// The key-set generation.
    pub generation: u64,
    /// The [`Trust::clock_epoch`].
    pub clock: u64,
}

impl Moment {
    /// The moment now.
    #[must_use]
    pub fn now(trust: &Trust) -> Self {
        Self {
            utc: trust.utc(),
            generation: trust.generation(),
            clock: trust.clock_epoch(),
        }
    }
}

/// Whether the servers had the UTC at some moment between the clock epochs
/// `from` and `to` (`to` not earlier).
#[must_use]
pub fn clock_known_between(from: u64, to: u64) -> bool {
    from.is_multiple_of(2) || to > from
}

/// Whether the servers lacked the UTC at some moment between the clock
/// epochs `from` and `to` (`to` not earlier).
#[must_use]
pub fn clock_absent_between(from: u64, to: u64) -> bool {
    !from.is_multiple_of(2) || to > from
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    next_subject: u64,
    issued: BTreeMap<u64, Issued>,
    receipts: BTreeMap<u64, Vec<Receipt>>,
    outcomes: BTreeMap<u64, Class>,
    /// Every credential a refreshing source minted for a request, with the
    /// moment of the mint.
    mints: BTreeMap<u64, Vec<(Minted, Moment)>>,
}

/// The run's ledger. Clones share it.
#[derive(Clone, Default)]
pub struct Ledger {
    inner: Arc<Mutex<Inner>>,
}

impl Ledger {
    /// The run's ledger, created on first use.
    #[must_use]
    pub fn of(state: &StateHandle) -> Self {
        if let Some(ledger) = state.get::<Self>(LEDGER_KEY) {
            return ledger;
        }
        let ledger = Self::default();
        state.publish(LEDGER_KEY, ledger.clone());
        ledger
    }

    /// A subject name never handed out before in this run.
    #[must_use]
    pub fn fresh_subject(&self, prefix: &str) -> String {
        let mut inner = lock(&self.inner);
        inner.next_subject += 1;
        format!("{prefix}-{}", inner.next_subject)
    }

    /// Record a request before it is sent; returns its id.
    #[must_use]
    pub fn issue(&self, issued: Issued) -> u64 {
        let mut inner = lock(&self.inner);
        inner.next_id += 1;
        let id = inner.next_id;
        inner.issued.insert(id, issued);
        id
    }

    /// Record that a handler received request `id`; returns whether the
    /// oracle allows it.
    #[must_use]
    pub fn receive(&self, id: u64, receipt: Receipt, trust: &Trust) -> bool {
        let mut inner = lock(&self.inner);
        let allowed = inner.issued.get(&id).is_some_and(|issued| {
            candidates(issued, inner.mints.get(&id))
                .iter()
                .any(|candidate| may_run(candidate, &receipt, trust))
        });
        inner.receipts.entry(id).or_default().push(receipt);
        allowed
    }

    /// What was issued as `id`.
    #[must_use]
    pub fn issued(&self, id: u64) -> Option<Issued> {
        lock(&self.inner).issued.get(&id).cloned()
    }

    /// How many times a handler received `id`.
    #[must_use]
    pub fn receipts(&self, id: u64) -> usize {
        lock(&self.inner).receipts.get(&id).map_or(0, Vec::len)
    }

    /// Record a credential a refreshing source minted for request `id`, at
    /// the current moment of `trust`.
    pub fn mint(&self, id: u64, minted: Minted, trust: &Trust) {
        lock(&self.inner)
            .mints
            .entry(id)
            .or_default()
            .push((minted, Moment::now(trust)));
    }

    /// The credentials request `id` may have carried: its own, or for a
    /// refreshing source every one it minted (each judged from its mint).
    #[must_use]
    pub fn carried(&self, id: u64) -> Vec<Issued> {
        let inner = lock(&self.inner);
        inner
            .issued
            .get(&id)
            .map(|issued| candidates(issued, inner.mints.get(&id)))
            .unwrap_or_default()
    }

    /// Record what the caller of `id` saw.
    pub fn outcome(&self, id: u64, class: Class) {
        lock(&self.inner).outcomes.insert(id, class);
    }

    /// Every issued id with its receipt count and its caller's outcome.
    #[must_use]
    pub fn all(&self) -> Vec<(u64, Issued, usize, Option<Class>)> {
        let inner = lock(&self.inner);
        inner
            .issued
            .iter()
            .map(|(id, issued)| {
                (
                    *id,
                    issued.clone(),
                    inner.receipts.get(id).map_or(0, Vec::len),
                    inner.outcomes.get(id).copied(),
                )
            })
            .collect()
    }
}

/// The credentials a request may have carried: for a refreshing source,
/// each minted one as if issued at its mint; otherwise the request's own.
fn candidates(issued: &Issued, mints: Option<&Vec<(Minted, Moment)>>) -> Vec<Issued> {
    if issued.minted.kind != Kind::Refreshing {
        return vec![issued.clone()];
    }
    mints
        .into_iter()
        .flatten()
        .map(|(minted, moment)| Issued {
            minted: minted.clone(),
            utc_sent: moment.utc,
            generation_sent: moment.generation,
            clock_sent: moment.clock,
            ..issued.clone()
        })
        .collect()
}

/// Whether the credential could have been accepted at some point between
/// its sending and `seen` (a receipt, or the caller seeing the outcome).
/// A server with no UTC fails closed: the servers must have had the time
/// at some moment in between.
#[must_use]
pub fn acceptable(issued: &Issued, seen: Moment, trust: &Trust) -> bool {
    let Moment {
        utc,
        generation,
        clock,
    } = seen;
    let minted = &issued.minted;
    match minted.kind {
        kind if kind.is_signed_by_the_issuer() => {
            clock_known_between(issued.clock_sent, clock) &&
            // Some instant in [utc_sent, utc] lies in [nbf, exp).
            minted.not_before <= utc
                && minted.expires > issued.utc_sent
                && minted.not_before.max(issued.utc_sent) < minted.expires
                && minted.key.is_some_and(|key| {
                    trust.published_between(key, issued.generation_sent, generation)
                })
        }
        _ => false,
    }
}

/// The oracle for one receipt.
fn may_run(issued: &Issued, receipt: &Receipt, trust: &Trust) -> bool {
    if issued.target != receipt.target {
        return false;
    }
    let verified = || {
        acceptable(
            issued,
            Moment {
                utc: receipt.utc,
                generation: receipt.generation,
                clock: receipt.clock,
            },
            trust,
        ) && receipt.subject.as_deref() == Some(issued.subject.as_str())
    };
    match issued.target {
        // A version 1 session carries no credential: anonymous, always.
        Target::LegacyPublic => receipt.subject.is_none(),
        Target::LegacyPrivate => false,
        Target::Public if issued.minted.kind == Kind::Anonymous => receipt.subject.is_none(),
        Target::Public => verified(),
        Target::Private | Target::Scan | Target::Note => {
            issued.minted.kind != Kind::Anonymous && verified()
        }
    }
}

/// Whether `reason` is what the server may say about this credential at
/// some point up to `seen` (when the caller saw it).
#[must_use]
pub fn consistent_denial(
    issued: &Issued,
    reason: CredentialError,
    seen: Moment,
    trust: &Trust,
) -> bool {
    let Moment {
        utc,
        generation,
        clock,
    } = seen;
    let minted = &issued.minted;
    let private = matches!(issued.target, Target::Private | Target::Scan | Target::Note);
    match (minted.kind, reason) {
        (Kind::Anonymous, CredentialError::Missing) => private,
        (Kind::Forged, CredentialError::BadSignature)
        | (Kind::UnknownKey | Kind::RotatedOut, CredentialError::UnknownKey)
        | (Kind::WrongAudience, CredentialError::WrongAudience)
        | (Kind::WrongIssuer, CredentialError::WrongIssuer)
        | (Kind::Symmetric, CredentialError::UnsupportedAlgorithm)
        | (Kind::Garbage, CredentialError::Malformed) => true,
        // Only if servers lacked the time at some moment between the
        // sending and the outcome, and only once the signature verified.
        (kind, CredentialError::ClockUnavailable) => {
            kind.is_signed_by_the_issuer() && clock_absent_between(issued.clock_sent, clock)
        }
        (kind, CredentialError::Expired) if kind.is_signed_by_the_issuer() => minted.expires <= utc,
        (kind, CredentialError::NotYetValid) if kind.is_signed_by_the_issuer() => {
            minted.not_before > issued.utc_sent
        }
        // The key is looked up before the signature, the issuer or the
        // audience are checked: any token naming a key that is no longer
        // published (two rotations can pass while a request waits for its
        // session) meets this first. Only the algorithm comes earlier.
        (kind, CredentialError::UnknownKey) if kind != Kind::Symmetric => minted
            .key
            .is_some_and(|key| !trust.published_between(key, generation, generation)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use moonpool_sim::StateHandle;

    use moonpool_rpc::security::CredentialError;

    use super::{Issued, Ledger, Moment, Receipt, Route, Target, consistent_denial};
    use crate::security::trust::{Kind, START_UTC, Trust};

    /// Regression: a receipt of a refreshing source's request was accepted
    /// whatever tokens it minted. It is now judged against the minted
    /// tokens: none minted, or only expired ones, means no receipt allowed.
    #[test]
    fn refreshed_receipts_are_judged_against_the_tokens_minted() {
        let state = StateHandle::new();
        let trust = Trust::of(&state).expect("trust");
        let ledger = Ledger::of(&state);
        let issue = || {
            ledger.issue(Issued {
                minted: trust.mint(Kind::Refreshing, "refresher", 120, 0),
                subject: "refresher".into(),
                target: Target::Private,
                route: Route::Remote,
                utc_sent: trust.utc(),
                generation_sent: trust.generation(),
                clock_sent: trust.clock_epoch(),
            })
        };
        let receipt = |utc| Receipt {
            target: Target::Private,
            subject: Some("refresher".into()),
            utc,
            generation: trust.generation(),
            clock: trust.clock_epoch(),
        };
        let unminted = issue();
        assert!(!ledger.receive(unminted, receipt(START_UTC), &trust));
        let minted = issue();
        ledger.mint(
            minted,
            trust.mint(Kind::Refreshing, "refresher", 120, 0),
            &trust,
        );
        assert!(ledger.receive(minted, receipt(START_UTC + 1), &trust));
        trust.advance(10_000);
        let lapsed = issue();
        // Its only token had expired when it was sent (a source handing
        // out a stale token): never acceptable, whenever it arrived.
        let mut old = trust.mint(Kind::Refreshing, "refresher", 120, 0);
        old.not_before = START_UTC;
        old.expires = START_UTC + 60;
        ledger.mint(lapsed, old, &trust);
        assert!(!ledger.receive(lapsed, receipt(START_UTC + 20_000), &trust));
    }

    /// Regression (#445): the oracle did not know when the servers had no
    /// UTC. A signed token that ran while the clock was absent the whole
    /// time passed, and a `ClockUnavailable` refusal passed at any time.
    #[test]
    fn a_signed_token_fails_closed_while_the_servers_have_no_utc() {
        let state = StateHandle::new();
        let trust = Trust::of(&state).expect("trust");
        let ledger = Ledger::of(&state);
        let issue = || {
            ledger.issue(Issued {
                minted: trust.mint(Kind::Valid, "member", 120, 0),
                subject: "member".into(),
                target: Target::Private,
                route: Route::Remote,
                utc_sent: trust.utc(),
                generation_sent: trust.generation(),
                clock_sent: trust.clock_epoch(),
            })
        };
        let receipt = || Receipt::now(Target::Private, Some("member".into()), &trust);
        let refused = |id| {
            let issued = ledger.issued(id).expect("issued");
            consistent_denial(
                &issued,
                CredentialError::ClockUnavailable,
                Moment::now(&trust),
                &trust,
            )
        };

        // The clock is known the whole time: it may run, and a refusal
        // for a missing clock is a lie.
        let known = issue();
        assert!(!refused(known));
        assert!(ledger.receive(known, receipt(), &trust));

        // Sent while known, the clock lost before the receipt: it may have
        // been verified in time, and it may also have been refused.
        let lost_meanwhile = issue();
        trust.forget();
        assert!(refused(lost_meanwhile));
        assert!(ledger.receive(lost_meanwhile, receipt(), &trust));

        // Sent and received while the clock is absent: it never runs.
        let absent = issue();
        assert!(refused(absent));
        assert!(!ledger.receive(absent, receipt(), &trust));

        // A forget while already forgotten changes nothing.
        let still_absent = issue();
        trust.forget();
        assert!(!ledger.receive(still_absent, receipt(), &trust));

        // Restored meanwhile: it may have been verified after the restore.
        let restored_meanwhile = issue();
        trust.restore();
        assert!(ledger.receive(restored_meanwhile, receipt(), &trust));
        let after = issue();
        assert!(!refused(after));
    }
}
