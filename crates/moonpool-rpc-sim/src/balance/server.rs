//! A balanced server: one RPC runtime per boot at `ip:port`, one `Work`
//! endpoint published in the ledger's directory, and a per-boot character
//! (fast or slow, faithful or divergent). Each job may be answered,
//! declined as temporarily behind, left to a broken promise, or held until
//! the caller gives up. Now and then the server destroys its endpoint and
//! publishes a fresh one, so callers holding the old reference meet a
//! destroyed endpoint next to healthy alternatives.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcHandle};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    TimeProvider, assert_always, assert_reachable,
};

use super::messages::{Done, Work, expected_value};
use super::state::{Ledger, Publication, Receipt};
use crate::foundations::state::Board;
use crate::foundations::{RPC_PORT, report_stats, rpc_config};

/// The server role.
pub struct BalanceServer;

/// What this boot is like.
#[derive(Clone, Copy)]
struct Character {
    slow: bool,
    divergent: bool,
}

#[async_trait]
impl Process for BalanceServer {
    fn name(&self) -> &'static str {
        "balance-server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let me = ctx.my_ip().to_string();
        let ledger = Ledger::of(ctx.state());
        let boot = ledger.boot(&me);
        let board = Board::of(ctx.state());
        let label = format!("balance-server@{me}#{boot:04}");
        for (old, probe) in board.probes(&format!("balance-server@{me}#"), &label) {
            assert_always!(
                probe.is_released(),
                "a restarted balance server's old runtime released everything",
                { "runtime" => old }
            );
        }
        let (driver, rpc) = RpcDriver::listen(
            ctx.providers().clone(),
            &format!("{me}:{RPC_PORT}"),
            rpc_config(),
        )
        .await
        .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let character = Character {
            slow: ctx.random().random_range(0..3u8) == 0,
            divergent: ctx.random().random_range(0..4u8) == 0,
        };
        let server = Server {
            me,
            boot,
            ledger,
            character,
        };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = server.serve(ctx, &rpc) => result,
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        };
        result
    }
}

struct Server {
    me: String,
    boot: u64,
    ledger: Ledger,
    character: Character,
}

impl Server {
    fn register(
        &self,
        rpc: &RpcHandle<SimProviders>,
        generation: u64,
    ) -> SimulationResult<RequestStream<Work>> {
        let (service, stream) = rpc
            .register::<Work>(AccessClass::Public)
            .map_err(|error| SimulationError::InvalidState(format!("register: {error}")))?;
        self.ledger.publish(
            &self.me,
            Publication {
                boot: self.boot,
                generation,
                reference: service.to_bytes(),
            },
        );
        Ok(stream)
    }

    async fn serve(&self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) -> SimulationResult<()> {
        let mut generation = 1;
        let mut stream = self.register(rpc, generation)?;
        tracing::info!(boot = self.boot, "rpc_balance_server_ready");
        let mut inflight = FuturesUnordered::new();
        let rotation = || Duration::from_millis(ctx.random().random_range(1500..6000u64));
        let mut rotate_at = ctx.time().now() + rotation();
        loop {
            let pause = ctx.time().sleep(rotate_at.saturating_sub(ctx.time().now()));
            moonpool_sim::select! {
                Some(IncomingRequest { request, reply }) = stream.next() => {
                    inflight.push(self.handle(ctx, request, reply));
                }
                Some(()) = inflight.next(), if !inflight.is_empty() => {}
                slept = pause => {
                    if slept.is_err() {
                        // The simulation is shutting down.
                        return Ok(());
                    }
                    // Destroy the endpoint and publish a fresh one: the old
                    // reference is refused from now on (EndpointNotFound).
                    generation += 1;
                    stream = self.register(rpc, generation)?;
                    assert_reachable!("rpc balance server destroyed its endpoint and republished");
                    rotate_at = ctx.time().now() + rotation();
                }
            }
        }
    }

    async fn handle(
        &self,
        ctx: &SimContext,
        request: super::messages::Job,
        reply: moonpool_rpc::ReplyHandle<Work>,
    ) {
        assert_always!(
            self.ledger.current_boot(&self.me) == self.boot,
            "no balance handler of an ended boot runs"
        );
        let random = ctx.random();
        let behind = random.random_bool(0.08);
        let drop_reply = !behind && random.random_bool(0.04);
        let hold = !behind && !drop_reply && random.random_bool(0.03);
        let value = if self.character.divergent {
            expected_value(request.id).wrapping_add(1)
        } else {
            expected_value(request.id)
        };
        self.ledger.receive(
            request.id,
            Receipt {
                server: self.me.clone(),
                boot: self.boot,
                declined: behind,
                value,
            },
        );
        let delay = if self.character.slow {
            random.random_range(60..400u64)
        } else {
            random.random_range(0..15u64)
        };
        if ctx
            .time()
            .sleep(Duration::from_millis(delay))
            .await
            .is_err()
        {
            return;
        }
        if drop_reply {
            // Executed, then the promise breaks: ambiguous for the caller.
            drop(reply);
            return;
        }
        if hold {
            // Executed, answered only after every caller gave up.
            if ctx.time().sleep(Duration::from_secs(5)).await.is_err() {
                return;
            }
            drop(reply);
            return;
        }
        let _ = reply.send(&Done {
            id: request.id,
            server: self.me.clone(),
            boot: self.boot,
            behind,
            value,
            penalty: if self.character.slow { 2.0 } else { 1.0 },
        });
    }
}
