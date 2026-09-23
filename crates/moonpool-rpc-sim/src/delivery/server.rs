//! The server: a dynamic `Execute` endpoint whose handler writes the
//! execution ledger first and then behaves as the job asks, and a
//! well-known `Directory` that tells callers the current incarnation's
//! reference.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver};
use moonpool_sim::{
    Process, SimContext, SimulationError, SimulationResult, StateHandle, TimeProvider,
    assert_always, assert_reachable,
};

use super::messages::{DIRECTORY_ID, Directory, Done, Execute, Listing, Mode};
use super::policy::delivery_config;
use super::state::{CRASH_REQUESTS_KEY, CUT_REQUESTS_KEY, CUTS_INSTALLED_KEY, SERVER_BOOTS_KEY};
use super::{RPC_PORT, report_stats};
use crate::foundations::state::{Board, Ledger, bump};

/// The server role: a fresh incarnation per boot, at the same address.
pub struct DeliveryServer;

#[async_trait]
impl Process for DeliveryServer {
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
                "a crashed delivery server released everything it owned"
            );
        }
        let address = format!("{}:{RPC_PORT}", ctx.my_ip());
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, delivery_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (execute_ref, execute) = rpc
            .register::<Execute>(AccessClass::Public)
            .map_err(register_error)?;
        let (_, directory) = rpc
            .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
            .map_err(register_error)?;
        let listing = Listing {
            boot,
            execute: execute_ref.to_bytes(),
        };
        tracing::info!(boot, "rpc_delivery_server_ready");

        let ledger = Ledger::of(&state);
        let serve = async {
            futures::join!(
                serve_execute(execute, &ledger, &state, ctx),
                serve_directory(directory, &listing),
                report_stats(&rpc, &board, &label, ctx),
            );
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

async fn serve_directory(mut stream: RequestStream<Directory>, listing: &Listing) {
    while let Some(IncomingRequest { reply, .. }) = stream.recv().await {
        let _ = reply.send(listing);
    }
}

async fn serve_execute(
    mut stream: RequestStream<Execute>,
    ledger: &Ledger,
    state: &StateHandle,
    ctx: &SimContext,
) {
    let mut inflight = FuturesUnordered::new();
    // Replies the process dies holding.
    let mut held = Vec::new();
    loop {
        moonpool_sim::select! {
            incoming = stream.next() => match incoming {
                Some(incoming) => {
                    if let Some(reply) = handle(incoming, ledger, state, ctx, &mut inflight) {
                        held.push(reply);
                    }
                }
                None => return,
            },
            Some(()) = inflight.next(), if !inflight.is_empty() => {}
        }
    }
}

type Inflight<'a> =
    FuturesUnordered<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>>;

/// Record the execution, then do what the job asks. Returns a reply handle
/// to hold until the process dies.
fn handle<'a>(
    IncomingRequest { request, reply }: IncomingRequest<Execute>,
    ledger: &Ledger,
    state: &'a StateHandle,
    ctx: &'a SimContext,
    inflight: &mut Inflight<'a>,
) -> Option<moonpool_rpc::ReplyHandle<Execute>> {
    // The ledger is written before anything else: it is the independent
    // record of execution the workload judges its outcomes against.
    let receipts = ledger.record(request.id);
    let done = Done {
        id: request.id,
        receipts,
    };
    let mode = match Mode::from_u32(request.mode) {
        // A retransmitted lost-reply job is answered normally.
        Some(Mode::LoseReply) if receipts > 1 => Mode::Reply,
        Some(mode) => mode,
        None => Mode::Reply,
    };
    match mode {
        Mode::Reply => {
            let _ = reply.send(&done);
        }
        Mode::Slow => {
            let delay = Duration::from_millis(u64::from(request.delay_ms));
            inflight.push(Box::pin(async move {
                let _ = ctx.time().sleep(delay).await;
                if !reply.send(&done) {
                    assert_reachable!("rpc delivery reply route gone before a slow reply");
                }
            }));
        }
        Mode::NeverReply => {
            assert_always!(
                reply.expects_reply(),
                "a two-way request carries a reply route"
            );
            reply.never_reply();
        }
        Mode::Broken => drop(reply),
        Mode::LoseReply => {
            // Ask the fault script to cut the caller off, wait until the
            // cut is in place, and answer into it: the reply is lost with
            // the connection. (Bounded: a script that stopped serving
            // leaves this a plain slow reply.)
            let request = bump(state, CUT_REQUESTS_KEY);
            inflight.push(Box::pin(async move {
                for _ in 0..2000 {
                    if state.get::<u64>(CUTS_INSTALLED_KEY).unwrap_or(0) >= request {
                        break;
                    }
                    let _ = ctx.time().sleep(Duration::from_millis(5)).await;
                }
                let _ = reply.send(&done);
            }));
        }
        Mode::Crash => {
            let _ = bump(state, CRASH_REQUESTS_KEY);
            return Some(reply);
        }
        Mode::ReplyThenCrash => {
            let _ = bump(state, CRASH_REQUESTS_KEY);
            let delay = Duration::from_millis(u64::from(request.delay_ms));
            inflight.push(Box::pin(async move {
                let _ = ctx.time().sleep(delay).await;
                let _ = reply.send(&done);
            }));
        }
    }
    None
}
