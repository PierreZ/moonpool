//! Issuing, sending and judging one request: shared by the workload and by
//! the server's own local caller, so local and remote requests are held to
//! the same oracle.

use moonpool_rpc::security::{Credential, CredentialError};
use moonpool_rpc::{ErrorReason, Execution, RpcError, RpcHandle, RpcMethod, ServiceRef};
use moonpool_sim::{SimProviders, assert_always, assert_sometimes};

use super::CALL_TIMEOUT;
use super::messages::{Answer, Probe};
use super::state::{Class, Issued, Ledger, Route, Target, consistent_denial};
use super::trust::{Kind, Minted, Trust};

/// One caller: where it records, whose trust material it uses, its route.
#[derive(Clone)]
pub struct Caller {
    /// The run's ledger.
    pub ledger: Ledger,
    /// The run's trust material.
    pub trust: Trust,
    /// Who is calling.
    pub route: Route,
}

/// What an outcome proves.
#[must_use]
pub fn classify<T>(outcome: &Result<T, RpcError>) -> Class {
    match outcome {
        Ok(_) => Class::Replied,
        Err(error) => match error.execution() {
            Execution::NotAdmitted => Class::NotAdmitted,
            Execution::Executed => Class::Executed,
            _ => Class::Maybe,
        },
    }
}

/// A short, deterministic description of an outcome for the history.
#[must_use]
pub fn describe<T>(outcome: &Result<T, RpcError>) -> String {
    match outcome {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{:?}/{:?}", error.reason(), error.execution()),
    }
}

impl Caller {
    /// Mint a credential of `kind` and record the request before it leaves.
    #[must_use]
    pub fn issue(&self, kind: Kind, target: Target, ttl: u64, delay: u64) -> (u64, Minted) {
        let subject = self
            .ledger
            .fresh_subject(&format!("{:?}", self.route).to_lowercase());
        let minted = self.trust.mint(kind, &subject, ttl, delay);
        let id = self.reissue(&minted, &subject, target);
        (id, minted)
    }

    /// Record another request carrying an already minted credential.
    #[must_use]
    pub fn reissue(&self, minted: &Minted, subject: &str, target: Target) -> u64 {
        self.ledger.issue(Issued {
            minted: minted.clone(),
            subject: subject.to_string(),
            target,
            route: self.route,
            utc_sent: self.trust.utc(),
            generation_sent: self.trust.generation(),
        })
    }

    /// Send request `id` once, with its credential, to `service`.
    ///
    /// # Errors
    ///
    /// The call's [`RpcError`].
    pub async fn unary<M>(
        &self,
        rpc: &RpcHandle<SimProviders>,
        service: &ServiceRef<M>,
        id: u64,
        hold_ms: u32,
    ) -> Result<Answer, RpcError>
    where
        M: RpcMethod<Request = Probe, Reply = Answer>,
    {
        let mut client = service.bind(rpc);
        if let Some(token) = self
            .ledger
            .issued(id)
            .and_then(|issued| issued.minted.token)
        {
            client = client.with_credentials(Credential::bearer(token));
        }
        client
            .try_get_reply_within(&Probe { id, hold_ms }, CALL_TIMEOUT)
            .await
    }

    /// Judge what the caller of `id` saw (the answer it got, if any): the
    /// denial must be the one the credential deserves, and the class is
    /// recorded for the end-of-run receipt check.
    #[must_use]
    pub fn judge(&self, id: u64, outcome: &Result<Option<Answer>, RpcError>) -> Class {
        let class = classify(outcome);
        self.ledger.outcome(id, class);
        let Some(issued) = self.ledger.issued(id) else {
            return class;
        };
        match outcome {
            Ok(answer) => self.judge_answer(id, &issued, answer.as_ref()),
            Err(error) => self.judge_error(id, &issued, error),
        }
        class
    }

    fn judge_answer(&self, id: u64, issued: &Issued, answer: Option<&Answer>) {
        let anonymous = issued.minted.kind == Kind::Anonymous
            || matches!(issued.target, Target::LegacyPublic | Target::LegacyPrivate);
        if let Some(answer) = answer {
            assert_always!(
                answer.id == id
                    && if anonymous {
                        answer.subject.is_empty()
                    } else {
                        answer.subject == issued.subject
                    },
                "a reply carries the principal its own request's credential proved",
                { "id" => id, "subject" => answer.subject.clone() }
            );
        }
        match (self.route, issued.target, anonymous) {
            (Route::Remote, Target::Private | Target::Scan, false) => {
                assert_sometimes!(
                    true,
                    "rpc security verified token reached a private endpoint"
                );
            }
            (Route::Local, Target::Private, false) => {
                assert_sometimes!(
                    true,
                    "rpc security verified token reached a private endpoint locally"
                );
            }
            (Route::Remote, Target::Public, true) => {
                assert_sometimes!(
                    true,
                    "rpc security anonymous caller reached a public endpoint"
                );
            }
            (_, Target::LegacyPublic, _) => {
                assert_sometimes!(true, "rpc security legacy v1 server answered a public call");
            }
            _ => {}
        }
        assert_always!(
            self.route != Route::Version1,
            "a version 1 client never gets through a verifying server"
        );
        assert_always!(
            issued.target != Target::LegacyPrivate,
            "a private endpoint of a version 1 session never answers"
        );
    }

    /// Whether a refusal is honest about admission. A refusal proves
    /// non-admission of its own attempt only: a reliable call (the only
    /// one that asks its credential source again) that was resent may
    /// have run from an earlier copy, so its refusal says
    /// [`Execution::MaybeExecuted`]. Any other refusal says
    /// [`Execution::NotAdmitted`].
    fn refusal_admits_nothing(&self, id: u64, issued: &Issued, error: &RpcError) -> bool {
        match error.execution() {
            Execution::NotAdmitted => true,
            Execution::MaybeExecuted => resent_reliable(issued, self.ledger.carried(id).len()),
            _ => false,
        }
    }

    fn judge_error(&self, id: u64, issued: &Issued, error: &RpcError) {
        match error.reason() {
            ErrorReason::Unauthenticated(reason) => {
                // A refusal proves non-admission of its own attempt; a
                // reliable call's earlier copy may still have run.
                assert_always!(
                    self.refusal_admits_nothing(id, issued, error),
                    "a credential refusal is never admitted"
                );
                if error.execution() == Execution::MaybeExecuted {
                    assert_sometimes!(
                        true,
                        "rpc security resent call refused after an earlier copy left"
                    );
                }
                // A refreshing source's refusal concerns one of the tokens
                // it minted; any of them may be the one refused.
                let consistent = self.ledger.carried(id).iter().any(|carried| {
                    consistent_denial(
                        carried,
                        *reason,
                        self.trust.utc(),
                        self.trust.generation(),
                        &self.trust,
                    )
                });
                assert_always!(
                    consistent,
                    "a credential is refused only for what is wrong with it",
                    { "kind" => format!("{:?}", issued.minted.kind), "reason" => reason.name() }
                );
                if self.route == Route::Local {
                    assert_sometimes!(true, "rpc security local call refused like a remote one");
                }
                if issued.target == Target::Public {
                    assert_sometimes!(
                        true,
                        "rpc security invalid credential refused on a public endpoint"
                    );
                }
                denial_gate(issued.minted.kind, *reason);
            }
            ErrorReason::PermissionDenied => {
                assert_always!(false, "the default policy never denies a verified caller");
            }
            ErrorReason::EndpointNotFound if issued.target == Target::LegacyPrivate => {
                assert_always!(
                    self.refusal_admits_nothing(id, issued, error),
                    "never admitted"
                );
                assert_sometimes!(
                    true,
                    "rpc security v1 session kept out of a private endpoint"
                );
            }
            ErrorReason::CredentialWithheld(_) => {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "a withheld credential never left"
                );
                assert_always!(
                    matches!(issued.target, Target::LegacyPublic | Target::LegacyPrivate),
                    "credentials are withheld only from version 1 sessions here"
                );
                assert_sometimes!(true, "rpc security credential withheld from a v1 session");
            }
            ErrorReason::ServerShuttingDown => {
                assert_always!(
                    self.refusal_admits_nothing(id, issued, error),
                    "never admitted"
                );
                assert_sometimes!(
                    true,
                    "rpc security request refused while the server drained"
                );
            }
            ErrorReason::ConnectFailed(_) if self.route == Route::Version1 => {
                assert_sometimes!(true, "rpc security v1 client refused at the handshake");
            }
            _ => {}
        }
    }
}

/// Whether request `issued`, which carried `carried` credentials, is a
/// reliable call that was sent more than once: each transmission of a
/// refreshing call asks its source for a new credential.
fn resent_reliable(issued: &Issued, carried: usize) -> bool {
    issued.minted.kind == Kind::Refreshing && carried >= 2
}

fn denial_gate(kind: Kind, reason: CredentialError) {
    match (kind, reason) {
        (Kind::Anonymous, CredentialError::Missing) => {
            assert_sometimes!(
                true,
                "rpc security anonymous call refused by a private endpoint"
            );
        }
        (Kind::Forged, _) => assert_sometimes!(true, "rpc security forged signature refused"),
        (Kind::UnknownKey, _) => assert_sometimes!(true, "rpc security unknown key refused"),
        (Kind::RotatedOut, _) => assert_sometimes!(true, "rpc security retired key refused"),
        (Kind::WrongAudience, _) => assert_sometimes!(true, "rpc security wrong audience refused"),
        (Kind::WrongIssuer, _) => assert_sometimes!(true, "rpc security wrong issuer refused"),
        (Kind::Symmetric, _) => {
            assert_sometimes!(true, "rpc security symmetric algorithm confusion refused");
        }
        (Kind::Garbage, _) => assert_sometimes!(true, "rpc security malformed credential refused"),
        (_, CredentialError::Expired) => {
            assert_sometimes!(true, "rpc security expired token refused");
        }
        (_, CredentialError::NotYetValid) => {
            assert_sometimes!(true, "rpc security not-yet-valid token refused");
        }
        (_, CredentialError::ClockUnavailable) => {
            assert_sometimes!(
                true,
                "rpc security token refused while the server had no UTC"
            );
        }
        (_, CredentialError::UnknownKey) => {
            assert_sometimes!(true, "rpc security token refused after its key rotated out");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use moonpool_rpc::{ErrorReason, Execution, RpcError};
    use moonpool_sim::StateHandle;

    use super::Caller;
    use crate::security::state::{Ledger, Route, Target};
    use crate::security::trust::{Kind, Trust};

    /// Regression (seed 8471474105033841448 of sim-rpc-security): a
    /// reliable call whose session was cut was sent again to a server that
    /// had begun to drain. The draining server refused the second copy,
    /// and the runtime reported `MaybeExecuted` because the first copy had
    /// left. That is honest, but the oracle accepted only `NotAdmitted` for
    /// a `ServerShuttingDown` refusal. Every refusal is now judged by the
    /// same rule: `NotAdmitted`, or `MaybeExecuted` for a reliable call
    /// that was sent more than once.
    #[test]
    fn a_refusal_of_a_resent_reliable_call_may_follow_an_earlier_copy() {
        let state = StateHandle::new();
        let caller = Caller {
            ledger: Ledger::of(&state),
            trust: Trust::of(&state).expect("trust"),
            route: Route::Remote,
        };
        let draining = |execution| RpcError::new(ErrorReason::ServerShuttingDown, execution);
        let judge = |id| {
            let issued = caller.ledger.issued(id).expect("issued");
            (
                caller.refusal_admits_nothing(id, &issued, &draining(Execution::NotAdmitted)),
                caller.refusal_admits_nothing(id, &issued, &draining(Execution::MaybeExecuted)),
                caller.refusal_admits_nothing(id, &issued, &draining(Execution::Executed)),
            )
        };
        let mint_again = |id| {
            caller.ledger.mint(
                id,
                caller.trust.mint(Kind::Refreshing, "refresher", 120, 0),
                caller.trust.utc(),
                caller.trust.generation(),
            );
        };

        let (reliable, _) = caller.issue(Kind::Refreshing, Target::Private, 120, 0);
        mint_again(reliable);
        // Sent once: no earlier copy, so only `NotAdmitted` is honest.
        assert_eq!(judge(reliable), (true, false, false));
        mint_again(reliable);
        // Sent twice: the earlier copy may have run.
        assert_eq!(judge(reliable), (true, true, false));

        // A call that is never resent must say `NotAdmitted`.
        let (plain, _) = caller.issue(Kind::Valid, Target::Private, 120, 0);
        assert_eq!(judge(plain), (true, false, false));
    }
}
