//! The two servers: the real one (the seed's client authentication mode,
//! peer identities trusted for its private endpoint) and the impostor (a
//! stray authority's certificate for the same names).

use async_trait::async_trait;
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::security::tls::{Tls, TlsServer};
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcConfig, RpcDriver, RpcMethod};
use moonpool_sim::{
    Process, SimContext, SimulationError, SimulationResult, assert_always, assert_reachable,
    assert_sometimes,
};

use super::{
    IMPOSTOR_REFS_KEY, MEMBER, Mode, Pki, PrivateEcho, Probe, PublicEcho, Refs, SERVER_REFS_KEY,
    Sender,
};
use crate::foundations::{RPC_PORT, rpc_config};

fn pki(ctx: &SimContext) -> SimulationResult<Pki> {
    let topology = ctx.topology();
    Pki::of_topology(
        ctx.state(),
        ctx.random(),
        topology.ips_in_group("server").into_iter().next(),
        topology.ips_in_group("impostor").into_iter().next(),
    )
}

fn tls_error(error: impl std::fmt::Display) -> SimulationError {
    SimulationError::InvalidState(format!("tls: {error}"))
}

/// Which server a handler belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Real,
    Impostor,
}

async fn serve<M>(mut stream: RequestStream<M>, role: Role, private: bool)
where
    M: RpcMethod<Request = Probe, Reply = Probe>,
{
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        let sender = Sender::of(request.sender);
        assert_always!(
            role == Role::Real,
            "a client that trusts the authority never reaches an impostor server",
            { "sender" => format!("{sender:?}") }
        );
        if private {
            let subject = reply.principal().map(|who| who.subject().to_string());
            assert_always!(
                sender == Some(Sender::Member) && subject.as_deref() == Some(MEMBER),
                "a private endpoint runs only a request whose session proved the member",
                { "sender" => format!("{sender:?}"), "subject" => format!("{subject:?}") }
            );
            assert_sometimes!(
                true,
                "rpc tls member reached a private endpoint over mutual TLS"
            );
        } else {
            let encrypted = reply
                .peer()
                .is_some_and(moonpool_rpc::PeerContext::is_encrypted);
            assert_always!(
                encrypted,
                "a request reaches a TLS server over an encrypted session"
            );
            if reply.peer().is_some_and(|peer| peer.identity().is_some()) {
                assert_reachable!("rpc tls public endpoint saw a verified client");
            }
        }
        let _ = reply.send(&request);
    }
}

async fn run_server(
    ctx: &SimContext,
    tls: TlsServer,
    security: SecurityConfig,
    refs_key: &'static str,
    role: Role,
) -> SimulationResult<()> {
    let config = RpcConfig {
        security,
        ..rpc_config()
    };
    let address = format!("{}:{RPC_PORT}", ctx.my_ip());
    let (driver, rpc) =
        RpcDriver::listen_with(ctx.providers().clone(), &address, config, Tls::server(tls))
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
    let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
    let (private_ref, private) = rpc
        .register::<PrivateEcho>(AccessClass::Private)
        .map_err(register_error)?;
    let (public_ref, public) = rpc
        .register::<PublicEcho>(AccessClass::Public)
        .map_err(register_error)?;
    ctx.state().publish(
        refs_key,
        Refs {
            private: private_ref.to_bytes(),
            public: public_ref.to_bytes(),
        },
    );
    let serve = async {
        futures::join!(serve(private, role, true), serve(public, role, false));
    };
    let run = driver.run();
    futures::pin_mut!(run, serve);
    moonpool_sim::select! {
        error = &mut run => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
        () = &mut serve => Ok(()),
        () = ctx.shutdown().cancelled() => Ok(()),
    }
}

/// The real server.
pub struct TlsServerProcess;

#[async_trait]
impl Process for TlsServerProcess {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let pki = pki(ctx)?;
        let (issued, mode) = pki.with(|inner| (inner.server.clone(), inner.mode));
        let tls = TlsServer::new(issued.chain.clone(), issued.key(), pki.clock())
            .and_then(|tls| tls.with_client_auth(mode.client_auth(pki.client_roots())))
            .map_err(tls_error)?;
        pki.with(|inner| inner.server_tls = Some(tls.clone()));
        tracing::info!(?mode, "rpc_tls_server_ready");
        if mode == Mode::Off {
            assert_reachable!("rpc tls server requests no client certificate");
        }
        run_server(
            ctx,
            tls,
            SecurityConfig::default().admit_authenticated_peers(),
            SERVER_REFS_KEY,
            Role::Real,
        )
        .await
    }
}

/// The impostor: same names, a stray authority.
pub struct ImpostorProcess;

#[async_trait]
impl Process for ImpostorProcess {
    fn name(&self) -> &'static str {
        "impostor"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let pki = pki(ctx)?;
        let issued = pki.with(|inner| inner.impostor.clone());
        let tls =
            TlsServer::new(issued.chain.clone(), issued.key(), pki.clock()).map_err(tls_error)?;
        // It admits anyone: whatever reaches it is the client's failure.
        run_server(
            ctx,
            tls,
            SecurityConfig::trusted_network(),
            IMPOSTOR_REFS_KEY,
            Role::Impostor,
        )
        .await
    }
}
