//! The verifying server: one runtime per boot whose security verifies JWTs
//! with the production adapter against the shared, rotating JWKS and the
//! scripted UTC. Every handler writes the receipt ledger the moment it gets
//! a request and asserts the oracle allows it. A local caller inside the
//! same runtime sends its own requests through the same check. On a
//! graceful shutdown the runtime drains within a short deadline while the
//! handlers keep answering.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::{
    AccessClass, IncomingRequest, RequestStream, RpcConfig, RpcDriver, RpcHandle, RpcMethod,
    ServiceRef,
};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    assert_always, assert_sometimes,
};

use super::judge::Caller;
use super::messages::{
    Answer, PrivateEcho, PrivateNote, PrivateScan, Probe, PublicEcho, SCAN_ITEMS,
};
use super::state::{
    DRAINING_KEY, Ledger, Receipt, Route, SERVER_BOOTS_KEY, SERVER_REFS_KEY, ServerRefs, Target,
};
use super::trust::{Kind, Trust};
use super::{RPC_PORT, pause};
use crate::foundations::state::{Board, bump};
use crate::foundations::{report_stats, rpc_config};

/// The server role.
pub struct SecurityServer;

/// Where every handler of one boot records.
#[derive(Clone)]
struct Recorder {
    ledger: Ledger,
    trust: Trust,
}

impl Recorder {
    /// Record a receipt; the oracle must allow it.
    fn receive(&self, id: u64, target: Target, subject: Option<&str>) {
        let allowed = self.ledger.receive(
            id,
            Receipt {
                target,
                subject: subject.map(ToString::to_string),
                utc: self.trust.utc(),
                generation: self.trust.generation(),
            },
            &self.trust,
        );
        assert_always!(
            allowed,
            "no request reaches a handler without a credential its endpoint accepts",
            { "id" => id, "target" => format!("{target:?}"), "subject" => format!("{subject:?}") }
        );
    }
}

#[async_trait]
impl Process for SecurityServer {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let boot = bump(&state, SERVER_BOOTS_KEY);
        let board = Board::of(&state);
        let label = format!("server#{boot}");
        for (_, probe) in board.probes("server#", &label) {
            assert_always!(
                probe.is_released(),
                "a stopped server runtime released everything it owned"
            );
        }
        let trust = Trust::of(&state)?;
        let recorder = Recorder {
            ledger: Ledger::of(&state),
            trust: trust.clone(),
        };
        let security = SecurityConfig::enforced(trust.verifier(4)?)
            .accept_credentials_over_plaintext()
            .with_clock(trust.clock());
        let config = RpcConfig {
            security,
            ..rpc_config()
        };
        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, config)
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (private_ref, private) = rpc
            .register::<PrivateEcho>(AccessClass::Private)
            .map_err(register_error)?;
        let (public_ref, public) = rpc
            .register::<PublicEcho>(AccessClass::Public)
            .map_err(register_error)?;
        let (scan_ref, scan) = rpc
            .register::<PrivateScan>(AccessClass::Private)
            .map_err(register_error)?;
        let (note_ref, note) = rpc
            .register::<PrivateNote>(AccessClass::Private)
            .map_err(register_error)?;
        state.publish(
            SERVER_REFS_KEY,
            ServerRefs {
                boot,
                private: private_ref.to_bytes(),
                public: public_ref.to_bytes(),
                scan: scan_ref.to_bytes(),
                note: note_ref.to_bytes(),
            },
        );
        tracing::info!(boot, "rpc_security_server_ready");

        let serve = async {
            futures::join!(
                serve_unary(private, Target::Private, &recorder, ctx),
                serve_unary(public, Target::Public, &recorder, ctx),
                serve_scan(scan, &recorder),
                serve_notes(note, &recorder),
                local_caller(ctx, &rpc, &private_ref, &recorder),
                report_stats(&rpc, &board, &label, ctx),
            );
        };
        let run = driver.run();
        futures::pin_mut!(run, serve);
        moonpool_sim::select! {
            error = &mut run => {
                return Err(SimulationError::IoError(format!("rpc driver: {error}")));
            }
            () = &mut serve => return Ok(()),
            () = ctx.shutdown().cancelled() => {}
        }
        // Graceful: close admission and drain while the handlers keep
        // answering, within a deadline shorter or longer than the work.
        let grace = Duration::from_millis(ctx.random().random_range(20..600));
        state.publish(DRAINING_KEY, boot);
        moonpool_sim::select! {
            error = &mut run => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = &mut serve => Ok(()),
            report = rpc.shutdown(grace) => {
                assert_always!(!report.already_stopped, "the driver ran through the shutdown");
                if report.drained {
                    assert_sometimes!(true, "rpc security graceful shutdown drained its work");
                } else {
                    assert_sometimes!(
                        true,
                        "rpc security graceful shutdown ended work left at its deadline"
                    );
                }
                tracing::info!(boot, ?report, "rpc_security_server_shut_down");
                Ok(())
            }
        }
    }
}

async fn serve_unary<M>(
    mut stream: RequestStream<M>,
    target: Target,
    recorder: &Recorder,
    ctx: &SimContext,
) where
    M: RpcMethod<Request = Probe, Reply = Answer>,
{
    let mut replies = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            incoming = stream.recv() => match incoming {
                Some(IncomingRequest { request, reply }) => {
                    let subject = reply.principal().map(|who| who.subject().to_string());
                    recorder.receive(request.id, target, subject.as_deref());
                    replies.push(async move {
                        let hold = Duration::from_millis(u64::from(request.hold_ms));
                        if !hold.is_zero() && pause(ctx, hold).await.is_err() {
                            reply.never_reply();
                            return;
                        }
                        let _ = reply.send(&Answer {
                            id: request.id,
                            subject: subject.unwrap_or_default(),
                        });
                    });
                }
                None => return,
            },
            Some(()) = replies.next(), if !replies.is_empty() => {}
        }
    }
}

async fn serve_scan(mut stream: RequestStream<PrivateScan>, recorder: &Recorder) {
    let mut producing = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            incoming = stream.recv() => match incoming {
                Some(IncomingRequest { request, reply }) => {
                    let subject = reply.principal().map(|who| who.subject().to_string());
                    recorder.receive(request.id, Target::Scan, subject.as_deref());
                    let Ok(producer) = reply.into_stream() else {
                        continue;
                    };
                    producing.push(async move {
                        let answer = Answer {
                            id: request.id,
                            subject: subject.unwrap_or_default(),
                        };
                        for _ in 0..SCAN_ITEMS {
                            if producer.send(&answer).await.is_err() {
                                return;
                            }
                        }
                        let _ = producer.finish();
                    });
                }
                None => return,
            },
            Some(()) = producing.next(), if !producing.is_empty() => {}
        }
    }
}

async fn serve_notes(mut stream: RequestStream<PrivateNote>, recorder: &Recorder) {
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        let subject = reply.principal().map(|who| who.subject().to_string());
        recorder.receive(request.id, Target::Note, subject.as_deref());
        assert_always!(!reply.expects_reply(), "a one-way request expects no reply");
    }
}

/// Calls this runtime's own private endpoint with drawn credentials: local
/// delivery passes the same check as remote delivery.
async fn local_caller(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    private: &ServiceRef<PrivateEcho>,
    recorder: &Recorder,
) {
    let caller = Caller {
        ledger: recorder.ledger.clone(),
        trust: recorder.trust.clone(),
        route: Route::Local,
    };
    loop {
        if pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(200..1000)),
        )
        .await
        .is_err()
        {
            return;
        }
        let kind = Kind::DRAWN[ctx.random().random_range(0..Kind::DRAWN.len())];
        let (id, _) = caller.issue(
            kind,
            Target::Private,
            ctx.random().random_range(1..120),
            ctx.random().random_range(1..60),
        );
        let outcome = caller.unary(rpc, private, id, 0).await;
        let _ = caller.judge(id, &outcome.map(Some));
    }
}
