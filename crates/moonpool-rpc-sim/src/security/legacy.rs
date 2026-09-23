//! The legacy server: a runtime pinned to protocol version 1 with the
//! default security (public endpoints answer anyone, private ones nobody).
//! It stands for an old build during a rolling upgrade: a version 1
//! session carries no credential, so its handlers must only ever see
//! anonymous callers, and its private endpoint must never answer.

use async_trait::async_trait;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcConfig, RpcDriver, RpcMethod};
use moonpool_sim::{Process, SimContext, SimulationError, SimulationResult, assert_always};

use super::RPC_PORT;
use super::messages::{Answer, PrivateEcho, Probe, PublicEcho};
use super::state::{LEGACY_BOOTS_KEY, LEGACY_REFS_KEY, Ledger, LegacyRefs, Receipt, Target};
use super::trust::Trust;
use crate::foundations::rpc_config;
use crate::foundations::state::bump;

/// The legacy (version 1) server role.
pub struct LegacyServer;

#[async_trait]
impl Process for LegacyServer {
    fn name(&self) -> &'static str {
        "legacy"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let boot = bump(&state, LEGACY_BOOTS_KEY);
        let trust = Trust::of(&state)?;
        let ledger = Ledger::of(&state);
        let config = RpcConfig {
            protocol_versions: 1..=1,
            ..rpc_config()
        };
        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, config)
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (public_ref, public) = rpc
            .register::<PublicEcho>(AccessClass::Public)
            .map_err(register_error)?;
        let (private_ref, private) = rpc
            .register::<PrivateEcho>(AccessClass::Private)
            .map_err(register_error)?;
        state.publish(
            LEGACY_REFS_KEY,
            LegacyRefs {
                public: public_ref.to_bytes(),
                private: private_ref.to_bytes(),
            },
        );
        tracing::info!(boot, "rpc_security_legacy_ready");
        let serve = async {
            futures::join!(
                serve(public, Target::LegacyPublic, &ledger, &trust),
                serve(private, Target::LegacyPrivate, &ledger, &trust),
            );
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

async fn serve<M>(mut stream: RequestStream<M>, target: Target, ledger: &Ledger, trust: &Trust)
where
    M: RpcMethod<Request = Probe, Reply = Answer>,
{
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        let subject = reply.principal().map(|who| who.subject().to_string());
        let allowed = ledger.receive(
            request.id,
            Receipt {
                target,
                subject,
                utc: trust.utc(),
                generation: trust.generation(),
            },
            trust,
        );
        assert_always!(
            allowed,
            "a version 1 session only ever reaches a public endpoint, anonymously"
        );
        let _ = reply.send(&Answer {
            id: request.id,
            subject: String::new(),
        });
    }
}
