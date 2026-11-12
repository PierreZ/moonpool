//! The campaign's ledger and its judge.
//!
//! The workload records each call before it leaves and the outcome it saw.
//! Each server records each request its handlers receive, with the
//! identity the session proved and the server boot. At the end of a run,
//! [`Ledger::check`] judges each call against the receipts and the mode.
//!
//! [`refusal`] is the oracle for a new session: from the certificates in
//! force during the whole call, it gives the reason why the call must not
//! complete, if there is one.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use moonpool_rpc::security::UtcTime;
use moonpool_rpc::security::tls::pki::{Issued, SubjectName, Validity};
use moonpool_sim::{StateHandle, assert_always, assert_sometimes};

use super::{MEMBER, Mode, Sender};
use crate::lock;
use crate::security::state::Class;

const LEDGER_KEY: &str = "rpc.tls.ledger";

/// Where a call goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The real server's private endpoint.
    Private,
    /// The real server's public endpoint.
    Public,
    /// The impostor's private endpoint.
    Impostor,
}

/// Which server received a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The real server.
    Real,
    /// The impostor.
    Impostor,
}

/// One call as the workload sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// Who sent it.
    pub sender: Sender,
    /// Where it went.
    pub target: Target,
    /// Whether it went on a session that the call opened itself.
    pub fresh: bool,
}

/// One request a handler received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The server that received it.
    pub role: Role,
    /// The sender tag in the request (the harness's own truth).
    pub sender: Option<Sender>,
    /// The identity that the session proved, if any.
    pub identity: Option<String>,
    /// The boot of the server that received it.
    pub boot: u64,
}

#[derive(Default)]
struct Inner {
    calls: BTreeMap<u64, Call>,
    outcomes: BTreeMap<u64, Class>,
    receipts: BTreeMap<u64, Vec<Receipt>>,
    boots: u64,
}

/// The run's ledger. Clones share it.
#[derive(Clone, Default)]
pub struct Ledger(Arc<Mutex<Inner>>);

impl Ledger {
    /// The run's ledger, made on first use.
    #[must_use]
    pub fn of(state: &StateHandle) -> Self {
        if let Some(ledger) = state.get::<Self>(LEDGER_KEY) {
            return ledger;
        }
        let ledger = Self::default();
        state.publish(LEDGER_KEY, ledger.clone());
        ledger
    }

    /// Record a new boot of the real server; returns its number (1 first).
    #[must_use]
    pub fn boot(&self) -> u64 {
        let mut inner = lock(&self.0);
        inner.boots += 1;
        inner.boots
    }

    /// How many times the real server booted.
    #[must_use]
    pub fn boots(&self) -> u64 {
        lock(&self.0).boots
    }

    /// Record call `id` before it leaves.
    pub fn call(&self, id: u64, call: Call) {
        lock(&self.0).calls.insert(id, call);
    }

    /// Record what the caller of `id` saw.
    pub fn outcome(&self, id: u64, class: Class) {
        lock(&self.0).outcomes.insert(id, class);
    }

    /// Record that a handler received request `id`.
    pub fn receive(&self, id: u64, receipt: Receipt) {
        lock(&self.0).receipts.entry(id).or_default().push(receipt);
    }

    /// Judge each call of the run against the receipts, for the seed's
    /// `mode`. `stranger_presents` tells if the stranger has a certificate.
    pub fn check(&self, mode: Mode, stranger_presents: bool) {
        let inner = lock(&self.0);
        for (id, receipts) in &inner.receipts {
            assert_always!(
                inner.calls.contains_key(id),
                "a handler receives only a request that the workload sent",
                { "id" => *id }
            );
            for receipt in receipts {
                check_receipt(*id, receipt, mode, stranger_presents);
            }
        }
        for (id, call) in &inner.calls {
            let receipts = inner.receipts.get(id).map_or(0, Vec::len);
            assert_always!(
                receipts <= 1,
                "an at-most-once request ran at most once",
                { "id" => *id, "receipts" => receipts }
            );
            match inner.outcomes.get(id) {
                Some(Class::Replied | Class::Executed) => {
                    assert_always!(
                        receipts == 1,
                        "a request that produced an outcome reached its handler",
                        { "id" => *id, "target" => format!("{:?}", call.target) }
                    );
                }
                Some(Class::NotAdmitted) => {
                    assert_always!(
                        receipts == 0,
                        "a request reported not admitted never reached a handler",
                        { "id" => *id }
                    );
                    if call.target == Target::Private && call.sender != Sender::Member {
                        assert_sometimes!(
                            true,
                            "rpc tls request kept out of a private endpoint was not admitted"
                        );
                    }
                }
                Some(Class::Maybe) | None => {}
            }
        }
    }
}

/// The rules for one receipt: what the mode and the identity permit.
fn check_receipt(id: u64, receipt: &Receipt, mode: Mode, stranger_presents: bool) {
    let sender = receipt.sender;
    let identity = receipt.identity.as_deref();
    if receipt.role == Role::Impostor {
        // The handler asserts at once that nothing reaches it.
        return;
    }
    let detail = || format!("{id} {sender:?} {identity:?} boot {}", receipt.boot);
    assert_always!(
        identity.is_none() || (identity == Some(MEMBER) && sender == Some(Sender::Member)),
        "a session proves the member identity only for the member",
        { "receipt" => detail() }
    );
    let admitted = match mode {
        // No certificate requested: no session proves an identity.
        Mode::Off => identity.is_none(),
        // A presented certificate is verified: a stray one never passes.
        Mode::Optional => !(stranger_presents && sender == Some(Sender::Stranger)),
        // Mutual TLS: only a client with a certificate of the authority.
        Mode::Required => sender == Some(Sender::Member),
    };
    assert_always!(
        admitted,
        "a server admits only the callers its client authentication mode permits",
        { "mode" => format!("{mode:?}"), "receipt" => detail() }
    );
    if receipt.boot > 1 {
        assert_sometimes!(true, "rpc tls restarted server ran a request");
    }
}

/// Why a new session must not complete a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The server's leaf expired before the call started.
    ServerExpired,
    /// The server's leaf became valid only after the call ended.
    ServerNotValidYet,
    /// The server's leaf is not for a server.
    ServerWrongUse,
    /// The server's leaf does not name the address the client dialed.
    ServerNameMissing,
    /// The client's leaf comes from an authority the server does not trust.
    ClientUntrusted,
    /// The server's client roots moved to an authority nobody holds a
    /// leaf from.
    ClientRootsMoved,
    /// The client's leaf expired before the call started.
    ClientExpired,
    /// The client's leaf became valid only after the call ended.
    ClientNotValidYet,
    /// The client's leaf is not for a client.
    ClientWrongUse,
    /// Mutual TLS, and the client presents nothing.
    NoClientCertificate,
    /// The private endpoint, and the session cannot prove the member.
    NotMember,
}

/// The client side of a new session.
#[derive(Debug, Clone, Copy)]
pub enum Presented<'a> {
    /// No certificate.
    Nothing,
    /// A leaf of the real authority.
    Authority(&'a Issued),
    /// A leaf of the stray authority.
    Stray(&'a Issued),
}

/// What was in force during the whole call: the call started at
/// `utc_from` and ended at `utc_to` (the scripted UTC only moves forward),
/// and nothing else changed meanwhile.
#[derive(Debug, Clone, Copy)]
pub struct Session<'a> {
    /// The seed's mode.
    pub mode: Mode,
    /// The server's leaf.
    pub server: &'a Issued,
    /// The address the client dialed.
    pub server_ip: IpAddr,
    /// Whether the server's client roots moved away.
    pub roots_swapped: bool,
    /// What the client presents.
    pub presented: Presented<'a>,
    /// The UTC when the call started.
    pub utc_from: u64,
    /// The UTC when the call ended.
    pub utc_to: u64,
}

/// Whether `validity` misses the whole interval: `Some(true)` when it ended
/// before, `Some(false)` when it started after.
fn outside(validity: Validity, from: u64, to: u64) -> Option<bool> {
    if validity.not_after < UtcTime::from_unix_seconds(from) {
        Some(true)
    } else if validity.not_before > UtcTime::from_unix_seconds(to) {
        Some(false)
    } else {
        None
    }
}

/// The reason why a new session for `session` must not complete a call to
/// `target` (the real server), or `None` when it may.
#[must_use]
pub fn refusal(session: &Session<'_>, target: Target) -> Option<Refusal> {
    let server = session.server;
    match outside(server.validity, session.utc_from, session.utc_to) {
        Some(true) => return Some(Refusal::ServerExpired),
        Some(false) => return Some(Refusal::ServerNotValidYet),
        None => {}
    }
    if !server.usage.serves() {
        return Some(Refusal::ServerWrongUse);
    }
    if !server.names.contains(&SubjectName::Ip(session.server_ip)) {
        return Some(Refusal::ServerNameMissing);
    }
    let member = match (session.mode, session.presented) {
        (Mode::Required, Presented::Nothing) => return Some(Refusal::NoClientCertificate),
        // No certificate requested (no identity, whatever the client has),
        // or none presented where it is optional.
        (Mode::Off, _) | (Mode::Optional, Presented::Nothing) => false,
        (_, Presented::Stray(_)) => return Some(Refusal::ClientUntrusted),
        (_, Presented::Authority(leaf)) => {
            if session.roots_swapped {
                return Some(Refusal::ClientRootsMoved);
            }
            match outside(leaf.validity, session.utc_from, session.utc_to) {
                Some(true) => return Some(Refusal::ClientExpired),
                Some(false) => return Some(Refusal::ClientNotValidYet),
                None => {}
            }
            if !leaf.usage.dials() {
                return Some(Refusal::ClientWrongUse);
            }
            leaf.names.first() == Some(&SubjectName::Uri(MEMBER.into()))
        }
    };
    (target == Target::Private && !member).then_some(Refusal::NotMember)
}

#[cfg(test)]
mod tests {
    use moonpool_rpc::security::UtcTime;
    use moonpool_rpc::security::tls::pki::{
        Authority, Issued, KeySeed, LeafSpec, SubjectName, Usage, Validity,
    };

    use super::{Mode, Presented, Refusal, Session, Target, refusal};
    use crate::tls::{MEMBER, SERVER};

    const NOW: u64 = 1_900_000_000;

    fn leaf(authority: &Authority, names: Vec<SubjectName>, usage: Usage, start: u64) -> Issued {
        authority
            .issue(&LeafSpec {
                names,
                validity: Validity::starting(UtcTime::from_unix_seconds(start), 3600),
                usage,
                seed: KeySeed::Bytes([7; 32]),
            })
            .expect("leaf")
    }

    fn authority(byte: u8) -> Authority {
        let validity = Validity::starting(UtcTime::from_unix_seconds(NOW - 3600), 86_400);
        Authority::new("ca", validity, KeySeed::Bytes([byte; 32])).expect("authority")
    }

    /// The judge refuses each bad certificate, and only those.
    #[test]
    fn a_new_session_is_refused_only_for_a_bad_certificate() {
        let real = authority(1);
        let stray = authority(2);
        let ip = "10.0.0.1".parse().expect("ip");
        let server_names = vec![SubjectName::Uri(SERVER.into()), SubjectName::Ip(ip)];
        let member_names = vec![SubjectName::Uri(MEMBER.into())];
        let server = leaf(&real, server_names.clone(), Usage::Server, NOW - 60);
        let member = leaf(&real, member_names.clone(), Usage::Client, NOW - 60);
        let stranger = leaf(&stray, member_names.clone(), Usage::Client, NOW - 60);
        let session = Session {
            mode: Mode::Required,
            server: &server,
            server_ip: ip,
            roots_swapped: false,
            presented: Presented::Authority(&member),
            utc_from: NOW,
            utc_to: NOW + 1,
        };
        assert_eq!(refusal(&session, Target::Private), None);
        assert_eq!(
            refusal(
                &Session {
                    utc_from: NOW + 7200,
                    utc_to: NOW + 7201,
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ServerExpired)
        );
        let late = leaf(&real, member_names.clone(), Usage::Client, NOW + 600);
        assert_eq!(
            refusal(
                &Session {
                    presented: Presented::Authority(&late),
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ClientNotValidYet)
        );
        let wrong = leaf(&real, member_names, Usage::Server, NOW - 60);
        assert_eq!(
            refusal(
                &Session {
                    presented: Presented::Authority(&wrong),
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ClientWrongUse)
        );
        assert_eq!(
            refusal(
                &Session {
                    presented: Presented::Stray(&stranger),
                    mode: Mode::Optional,
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ClientUntrusted)
        );
        assert_eq!(
            refusal(
                &Session {
                    roots_swapped: true,
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ClientRootsMoved)
        );
        assert_eq!(
            refusal(
                &Session {
                    presented: Presented::Nothing,
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::NoClientCertificate)
        );
        // Mode off: anyone reaches the public endpoint, nobody the private.
        let off = Session {
            mode: Mode::Off,
            presented: Presented::Stray(&stranger),
            ..session
        };
        assert_eq!(refusal(&off, Target::Public), None);
        assert_eq!(refusal(&off, Target::Private), Some(Refusal::NotMember));
        let client_leaf = leaf(&real, server_names, Usage::Client, NOW - 60);
        assert_eq!(
            refusal(
                &Session {
                    server: &client_leaf,
                    ..session
                },
                Target::Public
            ),
            Some(Refusal::ServerWrongUse)
        );
    }
}
