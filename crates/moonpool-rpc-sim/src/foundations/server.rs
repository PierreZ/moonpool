//! The server process: echo, slow, crash-after-receipt and a rotating
//! ephemeral endpoint, every handler writing the receipt ledger first.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcHandle};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    StateHandle, TimeProvider, assert_always, assert_sometimes,
};

use super::messages::{CrashAfterReceipt, Echo, Echoed, Ephemeral, Probe, Slow};
use super::state::{
    Board, CRASH_REQUESTS_KEY, EPHEMERAL_KEY, Ledger, SERVER_BOOTS_KEY, SERVER_REFS_KEY,
    ServerRefs, bump,
};
use super::{RPC_PORT, report_stats, rpc_config};

/// The server role: one RPC runtime per boot, a fresh incarnation each time.
pub struct ServerProcess;

#[async_trait]
impl Process for ServerProcess {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let boot = bump(&state, SERVER_BOOTS_KEY);
        let board = Board::of(&state);
        let label = format!("server#{boot}");
        // Every earlier incarnation was aborted by a crash before this boot:
        // its driver, connections and child futures must all be gone.
        for (earlier, probe) in board.probes("server#", &label) {
            assert_always!(
                probe.is_released(),
                "a crashed server runtime released everything it owned"
            );
            tracing::debug!(%earlier, "earlier server incarnation released");
        }

        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, rpc_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (echo_ref, echo) = rpc
            .register::<Echo>(AccessClass::Public)
            .map_err(register_error)?;
        let (slow_ref, slow) = rpc
            .register::<Slow>(AccessClass::Public)
            .map_err(register_error)?;
        let (crash_ref, crash) = rpc
            .register::<CrashAfterReceipt>(AccessClass::Public)
            .map_err(register_error)?;
        // Publication is explicit application action: callers learn this
        // incarnation's references only from here.
        state.publish(
            SERVER_REFS_KEY,
            ServerRefs {
                boot,
                echo: echo_ref.to_bytes(),
                slow: slow_ref.to_bytes(),
                crash: crash_ref.to_bytes(),
            },
        );
        tracing::info!(boot, "rpc_server_ready");

        let ledger = Ledger::of(&state);
        let serve = async {
            futures::join!(
                serve_echo(echo, &ledger),
                serve_slow(slow, &ledger, ctx),
                serve_crash(crash, &ledger, &state),
                serve_ephemeral(&rpc, &ledger, &state),
                report_stats(&rpc, &board, &label, ctx),
            );
        };
        moonpool_sim::select! {
            () = driver.run() => Ok(()),
            () = serve => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

/// Record the receipt and check the body survived the network intact.
fn receive(ledger: &Ledger, request: &Probe) {
    let receipts = ledger.record(request.id);
    // Corrupted frames are never delivered: whatever reaches a handler is
    // exactly what the workload sent.
    assert_always!(
        request.is_intact(),
        "a handler only ever receives the exact request body that was sent"
    );
    assert_always!(receipts <= 1, "no request reaches a handler twice");
}

fn echoed(request: Probe) -> Echoed {
    Echoed {
        id: request.id,
        text: request.text,
    }
}

async fn serve_echo(mut stream: RequestStream<Echo>, ledger: &Ledger) {
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        receive(ledger, &request);
        let _ = reply.send(&echoed(request));
    }
}

async fn serve_slow(mut stream: RequestStream<Slow>, ledger: &Ledger, ctx: &SimContext) {
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        receive(ledger, &request);
        let delay = ctx.random().random_range(50..400);
        let _ = ctx.time().sleep(Duration::from_millis(delay)).await;
        if !reply.send(&echoed(request)) {
            // The caller's session died while the handler worked.
            assert_sometimes!(true, "rpc reply route gone before the reply");
        }
    }
}

async fn serve_crash(
    mut stream: RequestStream<CrashAfterReceipt>,
    ledger: &Ledger,
    state: &StateHandle,
) {
    // Held, never answered: the process dies with them.
    let mut held = Vec::new();
    while let Some(IncomingRequest { request, reply }) = stream.recv().await {
        receive(ledger, &request);
        let _ = bump(state, CRASH_REQUESTS_KEY);
        held.push(reply);
    }
}

/// Serve one request per ephemeral endpoint, destroy it, register the next.
/// The registry reuses the slot with a new generation, so a stale token that
/// still reached a handler would show up as a receipt the workload's oracle
/// forbids.
async fn serve_ephemeral(rpc: &RpcHandle<SimProviders>, ledger: &Ledger, state: &StateHandle) {
    let mut sequence = 0u64;
    loop {
        let Ok((service, mut stream)) = rpc.register::<Ephemeral>(AccessClass::Public) else {
            return;
        };
        sequence += 1;
        state.publish(EPHEMERAL_KEY, (sequence, service.to_bytes()));
        let Some(IncomingRequest { request, reply }) = stream.recv().await else {
            return;
        };
        receive(ledger, &request);
        drop(stream);
        let _ = reply.send(&echoed(request));
    }
}
