//! Echo server process for transport simulation.
//!
//! Implements the `Process` trait: runs a multi-method echo service on top of
//! `NetTransport`. Handles incoming RPC requests, echoing them back.

use async_trait::async_trait;
use tracing::instrument;

use moonpool_sim::{Process, SimContext, SimulationResult, assert_sometimes, buggify};
use moonpool_transport::{
    JsonCodec, NetTransport, NetTransportBuilder, Providers, ReplyPromise, TaskProvider,
    TimeProvider,
};

use crate::service::{
    EchoRequest, EchoResponse, METHOD_ECHO, METHOD_ECHO_DELAYED, METHOD_ECHO_OR_FAIL,
    parse_sim_addr,
};

/// Interface ID for the echo service (must match service.rs constant).
const ECHO_INTERFACE: u64 = 0xECE0_0001;

/// Echo server process. Created fresh on every boot via factory.
pub struct EchoServerProcess;

#[async_trait(?Send)]
impl Process for EchoServerProcess {
    fn name(&self) -> &str {
        "echo-server"
    }

    #[instrument(skip(self, ctx))]
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_ip = ctx.my_ip();
        let addr = parse_sim_addr(my_ip)?;

        // Build transport with listener for incoming connections
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(addr)
            .build_listening()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("transport build: {e}"))
            })?;

        // Register handlers for all 3 methods
        let (echo_stream, _) = NetTransport::register_handler_at::<EchoRequest, EchoResponse, _>(
            &transport,
            ECHO_INTERFACE,
            METHOD_ECHO,
            JsonCodec,
        );
        let (delayed_stream, _) = NetTransport::register_handler_at::<EchoRequest, EchoResponse, _>(
            &transport,
            ECHO_INTERFACE,
            METHOD_ECHO_DELAYED,
            JsonCodec,
        );
        let (fail_stream, _) = NetTransport::register_handler_at::<EchoRequest, EchoResponse, _>(
            &transport,
            ECHO_INTERFACE,
            METHOD_ECHO_OR_FAIL,
            JsonCodec,
        );

        tracing::info!(%my_ip, "echo server started, 3 methods registered");

        // Serve until shutdown
        let shutdown = ctx.shutdown().clone();
        loop {
            tokio::select! {
                Some((req, reply)) = echo_stream.recv() => {
                    handle_echo(&req, reply, my_ip);
                }
                Some((req, reply)) = delayed_stream.recv() => {
                    handle_echo_delayed(&req, reply, my_ip, ctx);
                }
                Some((req, reply)) = fail_stream.recv() => {
                    handle_echo_or_fail(&req, reply, my_ip);
                }
                _ = shutdown.cancelled() => {
                    tracing::info!("echo server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

/// Immediate echo: reply right away.
fn handle_echo(req: &EchoRequest, reply: ReplyPromise<EchoResponse, JsonCodec>, server_ip: &str) {
    assert_sometimes!(true, "echo_request_handled");
    reply.send(EchoResponse {
        seq_id: req.seq_id,
        client_id: req.client_id,
        server_ip: server_ip.to_string(),
    });
}

/// Delayed echo: schedule a response after a random simulated delay.
fn handle_echo_delayed(
    req: &EchoRequest,
    reply: ReplyPromise<EchoResponse, JsonCodec>,
    server_ip: &str,
    ctx: &SimContext,
) {
    let response = EchoResponse {
        seq_id: req.seq_id,
        client_id: req.client_id,
        server_ip: server_ip.to_string(),
    };

    let time = ctx.providers().time().clone();
    let delay_ms = moonpool_sim::sim_random_range(1..100);
    ctx.providers()
        .task()
        .spawn_task("echo_delayed_reply", async move {
            let _ = time
                .sleep(std::time::Duration::from_millis(delay_ms as u64))
                .await;
            assert_sometimes!(true, "echo_delayed_request_handled");
            reply.send(response);
        });
}

/// Echo with buggify-controlled failure: sometimes drops the promise (BrokenPromise).
fn handle_echo_or_fail(
    req: &EchoRequest,
    reply: ReplyPromise<EchoResponse, JsonCodec>,
    server_ip: &str,
) {
    if buggify!() {
        assert_sometimes!(true, "echo_or_fail_buggify_dropped_promise");
        drop(reply);
        return;
    }

    reply.send(EchoResponse {
        seq_id: req.seq_id,
        client_id: req.client_id,
        server_ip: server_ip.to_string(),
    });
}
