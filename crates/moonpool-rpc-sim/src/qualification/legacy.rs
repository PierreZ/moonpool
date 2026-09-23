//! The legacy peer: a runtime pinned to protocol version 1 with the
//! default security, standing for the previous build during a rolling
//! upgrade. It serves a public `Work` instance published through its
//! directory; the workload's current runtime (versions 1 and 2) must
//! interoperate with it, while the verifying servers (which advertise
//! version 2 only) refuse the workload's version 1 runtime at the
//! handshake.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::{AccessClass, IncomingRequest, RpcConfig, RpcDriver};
use moonpool_sim::{Process, SimContext, SimulationError, SimulationResult};

use super::messages::{DIRECTORY_ID, Directory, Publication, Work};
use super::server::{Recorder, hold_and_reply};
use super::state::{Instance, QualLedger};
use super::{RPC_PORT, check_previous_boots, qual_config};
use crate::foundations::report_stats;
use crate::foundations::state::Board;

/// The legacy (version 1) role.
pub struct Legacy;

#[async_trait]
impl Process for Legacy {
    fn name(&self) -> &'static str {
        "legacy"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let me = ctx.my_ip().to_string();
        let ledger = QualLedger::of(&state);
        let boot = ledger.boot(&me);
        let board = Board::of(&state);
        let label = check_previous_boots(&board, "legacy", &me, boot);
        let config = RpcConfig {
            protocol_versions: 1..=1,
            ..qual_config()
        };
        let address = format!("{me}:{RPC_PORT}");
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, config)
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (work_ref, mut work) = rpc
            .register::<Work>(AccessClass::Public)
            .map_err(register_error)?;
        let (_, mut directory) = rpc
            .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
            .map_err(register_error)?;
        let publication = Publication {
            server: me.clone(),
            boot,
            configuration: 0,
            work: work_ref.to_bytes(),
            scan: Vec::new(),
            private: Vec::new(),
        };
        ledger.publish(
            publication.work.clone(),
            Instance {
                process: me.clone(),
                boot,
                configuration: 0,
            },
        );
        tracing::info!(line = %format!("{me}#{boot} ready (v1)"), "rpc_qual_event");
        let recorder = Recorder { ledger, me, boot };
        let serve = async {
            let mut replies = FuturesUnordered::new();
            loop {
                moonpool_sim::select! {
                    Some(IncomingRequest { reply, .. }) = directory.next() => {
                        let _ = reply.send(&publication);
                    }
                    Some(IncomingRequest { request, reply }) = work.next() => {
                        let done = recorder.run(&request, 0);
                        replies.push(hold_and_reply(ctx, reply, request.hold_ms, done));
                    }
                    Some(()) = replies.next(), if !replies.is_empty() => {}
                }
            }
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}
