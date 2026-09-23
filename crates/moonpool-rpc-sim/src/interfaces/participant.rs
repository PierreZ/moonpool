//! A participant: one RPC runtime per boot at the same `ip:port`, a base
//! role instance published through its well-known directory, and a
//! recruiter that creates (and dismisses) a fresh role instance per
//! configuration.
//!
//! Everything a boot owns — the driver, every group, every stream, every
//! in-flight handler — lives inside the process body, so a crash drops it
//! all. Handlers check, at the moment they run, that the request's
//! reference names exactly this boot and instance.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_rpc::{AccessClass, IncomingRequest, RequestStream, RpcDriver, RpcError, RpcHandle};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    TimeProvider, assert_always, assert_reachable, buggify,
};

use super::messages::{
    Answer, DIRECTORY_ID, DISMISS_ID, Directory, Dismiss, Dismissed, Probe, Publication,
    RECRUITER_ID, Recruiter, Role, RoleRequest, RoleServer,
};
use super::state::{Instance, Ledger};
use crate::foundations::state::Board;
use crate::foundations::{RPC_PORT, report_stats, rpc_config};

/// The participant role; restarted at the same address by the fault script.
pub struct Participant;

/// One role instance's handler: who it is, and where it records runs.
struct RoleInstance {
    instance: Instance,
    ledger: Ledger,
}

impl RoleInstance {
    fn record(&self, request: &Probe) {
        let Instance {
            participant,
            boot,
            configuration,
        } = &self.instance;
        // The reference the caller used named a participant, a boot and an
        // instance (as published); the request may only run there.
        assert_always!(
            request.participant == *participant
                && request.boot == *boot
                && request.configuration == *configuration,
            "a request only runs in the boot and instance it names",
            {
                "id" => request.id,
                "expected_boot" => request.boot,
                "boot" => boot,
                "expected_configuration" => request.configuration,
                "configuration" => configuration
            }
        );
        // Every task of a crashed boot died with it.
        assert_always!(
            self.ledger.current_boot(participant) == *boot,
            "no handler of an ended boot runs after its process restarted"
        );
        let _ = self.ledger.execute(request.id, self.instance.clone());
    }
}

impl Role for RoleInstance {
    async fn status(&self, request: Probe) -> Answer {
        self.record(&request);
        Answer {
            id: request.id,
            participant: self.instance.participant.clone(),
            boot: self.instance.boot,
            configuration: self.instance.configuration,
        }
    }

    async fn note(&self, request: Probe) {
        self.record(&request);
    }
}

/// One boot's registered role instances, by configuration (0 is the base
/// instance), each with its handler.
struct Instances {
    me: String,
    boot: u64,
    ledger: Ledger,
    servers: BTreeMap<u64, (RoleServer, Arc<RoleInstance>)>,
    /// Where the next poll starts, so no instance starves the others.
    cursor: usize,
}

impl Instances {
    /// Register a fresh instance for `configuration` and record its
    /// publication.
    fn add(
        &mut self,
        rpc: &RpcHandle<SimProviders>,
        configuration: u64,
    ) -> Result<Publication, RpcError> {
        if let Some((server, _)) = self.servers.get(&configuration) {
            return Ok(self.publication(configuration, server));
        }
        let server = RoleServer::register(rpc, AccessClass::Public)?;
        let instance = Instance {
            participant: self.me.clone(),
            boot: self.boot,
            configuration,
        };
        self.ledger
            .publish(server.interface_ref().to_bytes(), instance.clone());
        let publication = self.publication(configuration, &server);
        let handler = Arc::new(RoleInstance {
            instance,
            ledger: self.ledger.clone(),
        });
        self.servers.insert(configuration, (server, handler));
        Ok(publication)
    }

    fn publication(&self, configuration: u64, server: &RoleServer) -> Publication {
        Publication {
            participant: self.me.clone(),
            boot: self.boot,
            configuration,
            role: Some(server.interface_ref()),
        }
    }

    /// The next role request of any instance, with its handler; instances
    /// are polled in rotation, starting after the one that yielded last.
    async fn next(&mut self) -> (Arc<RoleInstance>, RoleRequest) {
        futures::future::poll_fn(|cx| {
            let count = self.servers.len();
            for offset in 0..count {
                let slot = (self.cursor + offset) % count;
                let Some((server, handler)) = self.servers.values_mut().nth(slot) else {
                    continue;
                };
                if let Poll::Ready(Some(request)) = server.poll_next(cx) {
                    self.cursor = (slot + 1) % count;
                    return Poll::Ready((Arc::clone(handler), request));
                }
            }
            Poll::Pending
        })
        .await
    }
}

#[async_trait]
impl Process for Participant {
    fn name(&self) -> &'static str {
        "participant"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let me = ctx.my_ip().to_string();
        let ledger = Ledger::of(&state);
        let boot = ledger.boot(&me);
        let board = Board::of(&state);
        let label = format!("participant@{me}#{boot:04}");
        for (old, probe) in board.probes(&format!("participant@{me}#"), &label) {
            assert_always!(
                probe.is_released(),
                "a restarted participant's previous runtime released everything",
                {
                    "runtime" => old,
                    "alive" => probe.runtime_alive(),
                    "tasks" => probe.live_tasks(),
                    "connections" => probe.live_connections()
                }
            );
        }
        let address = format!("{me}:{RPC_PORT}");
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, rpc_config())
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let instances = Instances {
            me,
            boot,
            ledger,
            servers: BTreeMap::new(),
            cursor: 0,
        };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = serve(ctx, &rpc, instances) => result,
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
            () = ctx.shutdown().cancelled() => Ok(()),
        };
        result
    }
}

fn register_error(error: &RpcError) -> SimulationError {
    SimulationError::InvalidState(format!("register: {error}"))
}

async fn serve(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    mut instances: Instances,
) -> SimulationResult<()> {
    // Delayed registration: the listener is up, so references to earlier
    // boots are already refused as stale, but this boot has not published
    // anything yet (a directory lookup finds no endpoint).
    if buggify!() {
        let delay = ctx.random().random_range(1..800);
        assert_reachable!("rpc participant delayed its registrations after a boot");
        let _ = ctx.time().sleep(Duration::from_millis(delay)).await;
    }
    let base = instances
        .add(rpc, 0)
        .map_err(|error| register_error(&error))?;
    let (_, directory) = rpc
        .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (_, recruiter) = rpc
        .register_well_known::<Recruiter>(RECRUITER_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (_, dismiss) = rpc
        .register_well_known::<Dismiss>(DISMISS_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    tracing::info!(boot = instances.boot, "rpc_interfaces_participant_ready");
    serve_loop(rpc, &mut instances, &base, directory, recruiter, dismiss).await;
    Ok(())
}

async fn serve_loop(
    rpc: &RpcHandle<SimProviders>,
    instances: &mut Instances,
    base: &Publication,
    mut directory: RequestStream<Directory>,
    mut recruiter: RequestStream<Recruiter>,
    mut dismiss: RequestStream<Dismiss>,
) {
    let mut inflight = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            Some(IncomingRequest { reply, .. }) = directory.next() => {
                let _ = reply.send(base);
            }
            Some(IncomingRequest { request, reply }) = recruiter.next() => {
                // A configuration of 0 names the base instance: recruiting
                // it returns the base publication.
                if let Ok(publication) = instances.add(rpc, request.configuration) {
                    let _ = reply.send(&publication);
                }
            }
            Some(IncomingRequest { request, reply }) = dismiss.next() => {
                let removed = request.configuration != 0
                    && instances.servers.remove(&request.configuration).is_some();
                // Dropped before the answer leaves: the group refuses new
                // requests from now on.
                let _ = reply.send(&Dismissed { removed });
            }
            (handler, request) = instances.next() => {
                inflight.push(async move { RoleServer::dispatch(handler.as_ref(), request).await });
            }
            Some(()) = inflight.next(), if !inflight.is_empty() => {}
        }
    }
}
