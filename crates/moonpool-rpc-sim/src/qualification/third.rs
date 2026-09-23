//! The third participant: one runtime per boot at the same address. It
//! adopts recruited members whose interfaces arrive inside a request and
//! calls them through its own runtime, and it owns a callback endpoint
//! (fresh per boot, published through its directory) that servers call
//! when a request hands them its reference.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcHandle, ServiceRef};
use moonpool_sim::{Process, SimContext, SimProviders, SimulationError, SimulationResult};

use super::messages::{
    ADOPTER_ID, Adopt, Adopter, Callback, DIRECTORY_ID, Directory, Job, Publication, Relayed, Work,
};
use super::server::{Recorder, hold_and_reply};
use super::state::{Instance, QualLedger};
use super::{CALL_TIMEOUT, RPC_PORT, check_previous_boots, qual_config};
use crate::foundations::report_stats;
use crate::foundations::state::Board;

/// The third participant role.
pub struct Third;

#[async_trait]
impl Process for Third {
    fn name(&self) -> &'static str {
        "third"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let me = ctx.my_ip().to_string();
        let ledger = QualLedger::of(&state);
        let boot = ledger.boot(&me);
        let board = Board::of(&state);
        let label = check_previous_boots(&board, "third", &me, boot);
        let address = format!("{me}:{RPC_PORT}");
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, qual_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let register_error = |error| SimulationError::InvalidState(format!("register: {error}"));
        let (callback_ref, callbacks) = rpc
            .register::<Callback>(AccessClass::Public)
            .map_err(register_error)?;
        let (_, directory) = rpc
            .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
            .map_err(register_error)?;
        let (_, adopter) = rpc
            .register_well_known::<Adopter>(ADOPTER_ID, AccessClass::Public)
            .map_err(register_error)?;
        let publication = Publication {
            server: me.clone(),
            boot,
            configuration: 0,
            work: callback_ref.to_bytes(),
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
        tracing::info!(line = %format!("{me}#{boot} ready"), "rpc_qual_event");
        let recorder = Recorder { ledger, me, boot };
        let serve = async {
            futures::join!(
                serve_callbacks(ctx, callbacks, &recorder),
                serve_directory(directory, &publication),
                adopt(&rpc, adopter),
            );
        };
        moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = serve => Ok(()),
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        }
    }
}

async fn serve_callbacks(
    ctx: &SimContext,
    mut callbacks: RequestStream<Callback>,
    recorder: &Recorder,
) {
    let mut replies = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            incoming = callbacks.next() => match incoming {
                Some(IncomingRequest { request, reply }) => {
                    let done = recorder.run(&request, 0);
                    replies.push(hold_and_reply(ctx, reply, request.hold_ms, done));
                }
                None => return,
            },
            Some(()) = replies.next(), if !replies.is_empty() => {}
        }
    }
}

async fn serve_directory(mut directory: RequestStream<Directory>, publication: &Publication) {
    while let Some(IncomingRequest { reply, .. }) = directory.next().await {
        let _ = reply.send(publication);
    }
}

async fn adopt(rpc: &RpcHandle<SimProviders>, mut adopter: RequestStream<Adopter>) {
    while let Some(IncomingRequest { request, reply }) = adopter.next().await {
        let Adopt { members, ids } = request;
        let calls = members.iter().zip(ids).map(|(member, id)| async move {
            let job = Job {
                id,
                server: member.server.clone(),
                boot: member.boot,
                configuration: member.configuration,
                hold_ms: 0,
            };
            // The interface arrived as data; binding it here is the third
            // participant's own, explicit choice.
            let outcome = match ServiceRef::<Work>::from_bytes(&member.work) {
                Ok(target) => target
                    .bind(rpc)
                    .try_get_reply_within(&job, CALL_TIMEOUT)
                    .await
                    .map_err(|error| format!("{:?}/{:?}", error.reason(), error.execution())),
                Err(_) => Err("undecodable".to_string()),
            };
            (id, outcome)
        });
        let mut relayed = Relayed::default();
        for (id, outcome) in futures::future::join_all(calls).await {
            match outcome {
                Ok(done) => relayed.answers.push(done),
                Err(failure) => relayed.failures.push(format!("{id}:{failure}")),
            }
        }
        let _ = reply.send(&relayed);
    }
}
