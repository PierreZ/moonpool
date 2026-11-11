//! Three client runtimes that trust the real authority, calling both
//! servers' endpoints during the chaos, then checking the member and the
//! public endpoint recover once the script restored clean certificates.
//!
//! Some calls go on a new runtime made for the call alone: its session is
//! new, so the certificates in force during the call decide it, and
//! [`refusal`] says when it must not complete.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::security::tls::pki::{Flaw, Issued};
use moonpool_rpc::security::tls::{ServerNames, Tls, TlsClient};
use moonpool_rpc::{ErrorReason, RpcDriver, RpcError, RpcHandle, RpcMethod, ServiceRef};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, Workload,
    assert_always, assert_reachable, assert_sometimes,
};

use super::ledger::{Call, Ledger, Presented, Refusal, Session, Target, refusal};
use super::{
    IMPOSTOR_REFS_KEY, Mode, Pki, PrivateEcho, Probe, PublicEcho, Refs, SCRIPT_DONE_KEY,
    SERVER_REFS_KEY, Sender,
};
use crate::foundations::{CALL_TIMEOUT, rpc_config};
use crate::security::judge::classify;
use crate::streams::pause;

/// The campaign's workload.
#[derive(Debug, Default)]
pub struct TlsWorkload {
    next_id: u64,
}

struct Runtimes {
    member: RpcHandle<SimProviders>,
    stranger: RpcHandle<SimProviders>,
    anonymous: RpcHandle<SimProviders>,
}

impl Runtimes {
    fn of(&self, sender: Sender) -> &RpcHandle<SimProviders> {
        match sender {
            Sender::Member => &self.member,
            Sender::Stranger => &self.stranger,
            Sender::Anonymous => &self.anonymous,
        }
    }
}

/// Record why a refused call was refused, as far as the error says.
fn observe_refusal(error: &RpcError) {
    let detail = match error.reason() {
        ErrorReason::ConnectFailed(detail) => detail.as_str(),
        _ => return,
    };
    if detail.contains("Expired") {
        assert_reachable!("rpc tls connect refused: certificate expired");
    } else if detail.contains("NotValidYet") {
        assert_reachable!("rpc tls connect refused: certificate not valid yet");
    } else if detail.contains("UnknownIssuer") || detail.contains("BadSignature") {
        assert_reachable!("rpc tls connect refused: untrusted authority");
    } else if detail.contains("buggify") {
        assert_reachable!("rpc tls handshake aborted by a fault site");
    }
}

async fn call<M>(
    rpc: &RpcHandle<SimProviders>,
    bytes: &[u8],
    probe: &Probe,
) -> Option<Result<Probe, RpcError>>
where
    M: RpcMethod<Request = Probe, Reply = Probe>,
{
    let service = ServiceRef::<M>::from_bytes(bytes).ok()?;
    Some(
        service
            .bind(rpc)
            .try_get_reply_within(probe, CALL_TIMEOUT)
            .await,
    )
}

impl TlsWorkload {
    fn probe(&mut self, sender: Sender) -> Probe {
        self.next_id += 1;
        Probe {
            id: self.next_id,
            sender: sender.tag(),
        }
    }

    /// Send one probe from `sender` to `target` on `rpc`, recorded in the
    /// ledger, and judge what comes back.
    async fn send(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        call_of: Call,
    ) -> Option<Result<Probe, RpcError>> {
        let Call { sender, target, .. } = call_of;
        let key = if target == Target::Impostor {
            IMPOSTOR_REFS_KEY
        } else {
            SERVER_REFS_KEY
        };
        let refs = ctx.state().get::<Refs>(key)?;
        let probe = self.probe(sender);
        let ledger = Ledger::of(ctx.state());
        ledger.call(probe.id, call_of);
        let outcome = if target == Target::Public {
            call::<PublicEcho>(rpc, &refs.public, &probe).await
        } else {
            call::<PrivateEcho>(rpc, &refs.private, &probe).await
        }?;
        ledger.outcome(probe.id, classify(&outcome));
        match &outcome {
            Ok(reply) => {
                assert_always!(reply == &probe, "a reply echoes its own request");
                assert_always!(
                    target != Target::Impostor,
                    "a client that trusts the authority never gets an impostor's reply"
                );
                assert_always!(
                    target != Target::Private || sender == Sender::Member,
                    "only the member gets a reply from a private endpoint"
                );
                if target == Target::Public && sender == Sender::Anonymous {
                    assert_sometimes!(true, "rpc tls anonymous client reached a public endpoint");
                }
            }
            Err(error) => {
                observe_refusal(error);
                match (target, sender) {
                    (Target::Impostor, _) => {
                        assert_sometimes!(true, "rpc tls impostor server refused by its client");
                    }
                    (Target::Private, Sender::Stranger) => {
                        assert_sometimes!(true, "rpc tls stranger refused by a private endpoint");
                    }
                    _ => {}
                }
            }
        }
        Some(outcome)
    }

    /// One call on a sender's long-lived runtime.
    async fn once(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        sender: Sender,
        target: Target,
    ) -> Option<Result<Probe, RpcError>> {
        let call_of = Call {
            sender,
            target,
            fresh: false,
        };
        self.send(ctx, runtimes.of(sender), call_of).await
    }

    /// One call to the real server on a runtime made for it alone, so on a
    /// new session. When the server's leaf and client roots did not change
    /// during the call, the oracle knows what the session met, and a call
    /// that the certificates must refuse never completes.
    async fn fresh(
        &mut self,
        ctx: &SimContext,
        pki: &Pki,
        sender: Sender,
        target: Target,
    ) -> SimulationResult<()> {
        let presented = pki.with(|inner| match sender {
            Sender::Member => Some(inner.member.clone()),
            Sender::Stranger => inner.stranger.clone(),
            Sender::Anonymous => None,
        });
        let before = pki.snapshot();
        let (driver, rpc) = RpcDriver::client_only_with(
            ctx.providers().clone(),
            rpc_config(),
            Tls::client(client(pki, presented.clone())?),
        )
        .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))?;
        let call_of = Call {
            sender,
            target,
            fresh: true,
        };
        let outcome = moonpool_sim::select! {
            _ = driver.run() => None,
            outcome = self.send(ctx, &rpc, call_of) => outcome,
        };
        let Some(outcome) = outcome else {
            return Ok(());
        };
        let (generation, utc_to) = pki.generation_and_utc();
        let Some(server_ip) = before.server_ip else {
            return Ok(());
        };
        if generation != before.generation {
            return Ok(());
        }
        let presented = match (sender, presented.as_ref()) {
            (Sender::Member, Some(leaf)) => Presented::Authority(leaf),
            (Sender::Stranger, Some(leaf)) => Presented::Stray(leaf),
            _ => Presented::Nothing,
        };
        let session = Session {
            mode: before.mode,
            server: &before.server,
            server_ip,
            roots_swapped: before.roots_swapped,
            presented,
            utc_from: before.utc,
            utc_to,
        };
        let verdict = refusal(&session, target);
        if let Some(reason) = verdict {
            assert_always!(
                outcome.is_err(),
                "a new session never completes a call that its certificates must refuse",
                { "reason" => format!("{reason:?}"), "sender" => format!("{sender:?}") }
            );
            if outcome.is_err() {
                observe_verdict(reason);
            }
        } else if outcome.is_ok() && before.server.flaw == Some(Flaw::NameDropped) {
            assert_reachable!("rpc tls server leaf without its URI still proves its address");
        }
        Ok(())
    }

    async fn drive(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        pki: &Pki,
    ) -> SimulationResult<()> {
        while !ctx.state().contains(SCRIPT_DONE_KEY) {
            if ctx.shutdown().is_cancelled() {
                return Ok(());
            }
            let sender = Sender::ALL[ctx.random().random_range(0..Sender::ALL.len())];
            let target = match ctx.random().random_range(0..10u8) {
                0..=4 => Target::Private,
                5..=8 => Target::Public,
                _ => Target::Impostor,
            };
            if target != Target::Impostor && ctx.random().random_bool(0.3) {
                self.fresh(ctx, pki, sender, target).await?;
            } else {
                let _ = self.once(ctx, runtimes, sender, target).await;
            }
            if pause(
                ctx,
                Duration::from_millis(ctx.random().random_range(10..200)),
            )
            .await
            .is_err()
            {
                return Ok(());
            }
        }
        // The script restored clean certificates: the member (when the
        // server verifies clients) and the public endpoint must come back.
        let mut member_back = pki.mode() == Mode::Off;
        let mut public_back = false;
        for _ in 0..80 {
            if !member_back {
                let outcome = self
                    .once(ctx, runtimes, Sender::Member, Target::Private)
                    .await;
                if let Some(Ok(_)) = outcome {
                    member_back = true;
                    assert_sometimes!(true, "rpc tls member served after the faults");
                }
            }
            if !public_back {
                // Mutual TLS turns away a client without a certificate at
                // the handshake, public endpoint or not.
                let sender = if pki.mode() == Mode::Required {
                    Sender::Member
                } else {
                    Sender::Anonymous
                };
                let outcome = self.once(ctx, runtimes, sender, Target::Public).await;
                public_back = matches!(outcome, Some(Ok(_)));
            }
            if member_back && public_back {
                return Ok(());
            }
            if pause(ctx, Duration::from_millis(250)).await.is_err() {
                return Ok(());
            }
        }
        assert_always!(
            false,
            "the member and the public endpoint are served once the faults stopped",
            { "member" => member_back, "public" => public_back }
        );
        Ok(())
    }
}

/// Coverage: which certificate fault turned a new session away.
fn observe_verdict(reason: Refusal) {
    match reason {
        Refusal::ServerExpired => {
            assert_reachable!("rpc tls new session refused: server leaf expired");
        }
        Refusal::ServerNotValidYet => {
            assert_reachable!("rpc tls new session refused: server leaf not valid yet");
        }
        Refusal::ServerWrongUse => {
            assert_reachable!("rpc tls new session refused: server leaf for the wrong use");
        }
        Refusal::ServerNameMissing => {
            assert_reachable!("rpc tls new session refused: server leaf without the address");
        }
        Refusal::ClientUntrusted => {
            assert_sometimes!(true, "rpc tls new session refused: stray client authority");
        }
        Refusal::ClientRootsMoved => {
            assert_reachable!("rpc tls new session refused: client roots moved away");
        }
        Refusal::ClientExpired => {
            assert_reachable!("rpc tls new session refused: member leaf expired");
        }
        Refusal::ClientNotValidYet => {
            assert_reachable!("rpc tls new session refused: member leaf not valid yet");
        }
        Refusal::ClientWrongUse => {
            assert_reachable!("rpc tls new session refused: member leaf for the wrong use");
        }
        Refusal::NoClientCertificate => {
            assert_reachable!("rpc tls new session refused: no client certificate");
        }
        Refusal::NotMember => {
            assert_sometimes!(true, "rpc tls new session kept out of a private endpoint");
        }
    }
}

fn client(pki: &Pki, certificate: Option<Issued>) -> SimulationResult<TlsClient> {
    let tls = |error| SimulationError::InvalidState(format!("tls client: {error}"));
    // Dialed by address: the server's IP name proves it.
    let client = TlsClient::new([pki.authority_root()], pki.clock())
        .map_err(tls)?
        .with_server_names(ServerNames::IpAddress);
    match certificate {
        Some(issued) => client
            .with_certificate(issued.chain.clone(), issued.key())
            .map_err(tls),
        None => Ok(client),
    }
}

#[async_trait]
impl Workload for TlsWorkload {
    fn name(&self) -> &'static str {
        "rpc_tls_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topology = ctx.topology();
        let pki = Pki::of_topology(
            ctx.state(),
            ctx.random(),
            topology.ips_in_group("server").into_iter().next(),
            topology.ips_in_group("impostor").into_iter().next(),
        )?;
        let (member_cert, stranger_cert) =
            pki.with(|inner| (inner.member.clone(), inner.stranger.clone()));
        let member_tls = client(&pki, Some(member_cert))?;
        pki.with(|inner| inner.member_tls = Some(member_tls.clone()));
        let start = |tls: TlsClient| {
            RpcDriver::client_only_with(ctx.providers().clone(), rpc_config(), Tls::client(tls))
                .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))
        };
        let (member_driver, member) = start(member_tls)?;
        let (stranger_driver, stranger) = start(client(&pki, stranger_cert)?)?;
        let (anonymous_driver, anonymous) = start(client(&pki, None)?)?;
        let runtimes = Runtimes {
            member,
            stranger,
            anonymous,
        };
        moonpool_sim::select! {
            error = member_driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            error = stranger_driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            error = anonymous_driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = self.drive(ctx, &runtimes, &pki) => result,
        }
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let Some(pki) = ctx.state().get::<Pki>(super::PKI_KEY) else {
            return Ok(());
        };
        let (mode, stranger_presents) = pki.with(|inner| (inner.mode, inner.stranger.is_some()));
        let ledger = Ledger::of(ctx.state());
        ledger.check(mode, stranger_presents);
        if ledger.boots() > 1 {
            assert_reachable!("rpc tls server restarted during a run");
        }
        Ok(())
    }
}
