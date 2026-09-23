//! A server: one verifying runtime per boot at the same `ip:port`, serving
//! everything the other campaigns tested apart, at once:
//!
//! - a base `Work` instance (unary, one-way, reliable and balanced calls),
//!   and a fresh `Work` instance per recruited configuration;
//! - the streams campaign's `ScanItems` producer, writing its ledger;
//! - the security campaign's credential-protected `PrivateEcho`, verified
//!   by the production JWT/JWKS adapter over the rotating key set and the
//!   scripted UTC;
//! - well-known directory, recruiter and forwarder endpoints (the
//!   forwarder calls a callback owned by a third process).
//!
//! Every handler writes the execution ledger when user code gets the
//! request and asserts on the spot that the request's reference named this
//! very boot and instance, and that the boot is still current. A graceful
//! shutdown drains within a drawn deadline while the handlers keep
//! answering. Before touching the network, a boot checks that every
//! earlier runtime of its process returned to its baseline.

use std::collections::BTreeMap;
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::{
    AccessClass, IncomingRequest, RequestStream, RpcConfig, RpcDriver, RpcHandle, ServiceRef,
};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult,
    TimeProvider, assert_always, assert_reachable, assert_sometimes, buggify,
};

use super::messages::{
    Callback, DIRECTORY_ID, Directory, Done, FORWARDER_ID, Forward, Forwarder, Job, Publication,
    RECRUITER_ID, Recruiter, Relayed, Work,
};
use super::state::{Instance, QualLedger, SCRIPT_DONE_KEY, instance_code};
use super::{CALL_TIMEOUT, RPC_PORT, check_previous_boots, pause, qual_config};
use crate::foundations::report_stats;
use crate::foundations::state::Board;
use crate::security::judge::Caller;
use crate::security::messages::{Answer, PrivateEcho};
use crate::security::state::{Ledger as AuthLedger, Receipt, Route, Target};
use crate::security::trust::{Kind, Trust};
use crate::streams::messages::ScanItems;
use crate::streams::producer::produce;
use crate::streams::state::StreamLedger;

/// The server role.
pub struct QualServer;

/// Where one boot's handlers record, and who they are.
#[derive(Clone)]
pub(crate) struct Recorder {
    pub(crate) ledger: QualLedger,
    pub(crate) me: String,
    pub(crate) boot: u64,
}

impl Recorder {
    /// Record a run of `job` by `configuration`'s instance of this boot,
    /// checking the oracle on the spot.
    pub(crate) fn run(&self, job: &Job, configuration: u64) -> Done {
        let instance = Instance {
            process: self.me.clone(),
            boot: self.boot,
            configuration,
        };
        // An empty server is a balanced call over a set: the workload judges
        // it against the set's instances.
        if !job.server.is_empty() {
            assert_always!(
                job.server == instance.process
                    && job.boot == instance.boot
                    && job.configuration == instance.configuration,
                "rpc qual a request ran only in the instance it named",
                {
                    "id" => job.id,
                    "expected" => format!("{}#{}/{}", job.server, job.boot, job.configuration),
                    "ran" => instance.to_string()
                }
            );
        }
        assert_always!(
            self.ledger.current_boot(&self.me) == self.boot,
            "rpc qual no handler of an ended boot ran"
        );
        let _ = self.ledger.execute(job.id, instance);
        Done {
            id: job.id,
            server: self.me.clone(),
            boot: self.boot,
            configuration,
        }
    }
}

/// Answer `job` after its hold, or never if the simulation goes away.
pub(crate) async fn hold_and_reply(
    ctx: &SimContext,
    reply: moonpool_rpc::ReplyHandle<impl moonpool_rpc::RpcMethod<Reply = Done>>,
    hold_ms: u32,
    done: Done,
) {
    let hold = Duration::from_millis(u64::from(hold_ms));
    if !hold.is_zero() && pause(ctx, hold).await.is_err() {
        reply.never_reply();
        return;
    }
    let _ = reply.send(&done);
}

/// The boot's `Work` instances by configuration (0: the base instance).
struct Instances {
    streams: BTreeMap<u64, RequestStream<Work>>,
    cursor: usize,
}

impl Instances {
    /// The next request of any instance; instances are polled in rotation.
    async fn next(&mut self) -> (u64, IncomingRequest<Work>) {
        futures::future::poll_fn(|cx| {
            let count = self.streams.len();
            for offset in 0..count {
                let slot = (self.cursor + offset) % count;
                let Some((configuration, stream)) = self.streams.iter_mut().nth(slot) else {
                    continue;
                };
                if let Poll::Ready(Some(request)) = stream.poll_next_unpin(cx) {
                    self.cursor = (slot + 1) % count;
                    return Poll::Ready((*configuration, request));
                }
            }
            Poll::Pending
        })
        .await
    }
}

fn register_error(error: &moonpool_rpc::RpcError) -> SimulationError {
    SimulationError::InvalidState(format!("register: {error}"))
}

#[async_trait]
impl Process for QualServer {
    fn name(&self) -> &'static str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let state = ctx.state().clone();
        let me = ctx.my_ip().to_string();
        let ledger = QualLedger::of(&state);
        let boot = ledger.boot(&me);
        let board = Board::of(&state);
        let label = check_previous_boots(&board, "server", &me, boot);
        let trust = Trust::of(&state)?;
        let security = SecurityConfig::enforced(trust.verifier(4)?)
            .accept_credentials_over_plaintext()
            .with_clock(trust.clock());
        let config = RpcConfig {
            security,
            ..qual_config()
        };
        let address = format!("{me}:{RPC_PORT}");
        let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, config)
            .await
            .map_err(|error| SimulationError::IoError(format!("rpc listen: {error}")))?;
        if let Some(probe) = rpc.probe() {
            board.register_probe(&label, probe);
        }
        let recorder = Recorder {
            ledger: ledger.clone(),
            me: me.clone(),
            boot,
        };
        let serve = serve(ctx, &rpc, &recorder, &trust);
        let run = driver.run();
        futures::pin_mut!(run, serve);
        moonpool_sim::select! {
            error = &mut run => {
                return Err(SimulationError::IoError(format!("rpc driver: {error}")));
            }
            result = &mut serve => return result,
            () = report_stats(&rpc, &board, &label, ctx) => return Ok(()),
            () = ctx.shutdown().cancelled() => {}
        }
        // Graceful: close admission and drain while the handlers keep
        // answering, within a deadline shorter or longer than the work.
        ledger.draining(&me, boot);
        let owed = rpc.stats().map_or(0, |stats| stats.inflight_requests);
        let grace = Duration::from_millis(ctx.random().random_range(20..800));
        moonpool_sim::select! {
            error = &mut run => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = &mut serve => result,
            report = rpc.shutdown(grace) => {
                assert_always!(!report.already_stopped, "the driver ran through the shutdown");
                if report.drained && owed > 0 {
                    assert_sometimes!(true, "rpc qual graceful shutdown drained an in-flight call");
                }
                if report.replies_abandoned > 0 {
                    assert_sometimes!(
                        true,
                        "rpc qual graceful shutdown ended work at its deadline"
                    );
                }
                tracing::info!(line = %format!("{me}#{boot} shut down drained={}", report.drained), "rpc_qual_event");
                Ok(())
            }
        }
    }
}

type Pending<'a> = BoxFuture<'a, ()>;

/// Everything one boot registered.
struct Registered {
    base: Publication,
    private_ref: ServiceRef<PrivateEcho>,
    work: RequestStream<Work>,
    scans: RequestStream<ScanItems>,
    private: RequestStream<PrivateEcho>,
    directory: RequestStream<Directory>,
    recruiter: RequestStream<Recruiter>,
    forwarder: RequestStream<Forwarder>,
}

/// Register every endpoint of the boot and record its publication before
/// anything advertises it.
fn register(rpc: &RpcHandle<SimProviders>, recorder: &Recorder) -> SimulationResult<Registered> {
    let (work_ref, work) = rpc
        .register::<Work>(AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (scan_ref, scans) = rpc
        .register::<ScanItems>(AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (private_ref, private) = rpc
        .register::<PrivateEcho>(AccessClass::Private)
        .map_err(|error| register_error(&error))?;
    let (_, directory) = rpc
        .register_well_known::<Directory>(DIRECTORY_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (_, recruiter) = rpc
        .register_well_known::<Recruiter>(RECRUITER_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let (_, forwarder) = rpc
        .register_well_known::<Forwarder>(FORWARDER_ID, AccessClass::Public)
        .map_err(|error| register_error(&error))?;
    let base = Publication {
        server: recorder.me.clone(),
        boot: recorder.boot,
        configuration: 0,
        work: work_ref.to_bytes(),
        scan: scan_ref.to_bytes(),
        private: private_ref.to_bytes(),
    };
    let instance = Instance {
        process: recorder.me.clone(),
        boot: recorder.boot,
        configuration: 0,
    };
    recorder
        .ledger
        .publish_base(base.work.clone(), instance.clone());
    for bytes in [&base.scan, &base.private] {
        recorder.ledger.publish(bytes.clone(), instance.clone());
    }
    Ok(Registered {
        base,
        private_ref,
        work,
        scans,
        private,
        directory,
        recruiter,
        forwarder,
    })
}

/// Admit one credentialed request: the security oracle must allow the
/// receipt; the answer carries the verified subject.
fn private_request<'a>(
    ctx: &'a SimContext,
    trust: &Trust,
    IncomingRequest { request, reply }: IncomingRequest<PrivateEcho>,
) -> Pending<'a> {
    let subject = reply.principal().map(|who| who.subject().to_string());
    let allowed = AuthLedger::of(ctx.state()).receive(
        request.id,
        Receipt {
            target: Target::Private,
            subject: subject.clone(),
            utc: trust.utc(),
            generation: trust.generation(),
        },
        trust,
    );
    assert_always!(
        allowed,
        "no request reaches a handler without a credential its endpoint accepts",
        { "id" => request.id, "subject" => format!("{subject:?}") }
    );
    Box::pin(async move {
        let hold = Duration::from_millis(u64::from(request.hold_ms));
        if !hold.is_zero() && pause(ctx, hold).await.is_err() {
            reply.never_reply();
            return;
        }
        let _ = reply.send(&Answer {
            id: request.id,
            subject: subject.unwrap_or_default(),
        });
    })
}

async fn serve(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    recorder: &Recorder,
    trust: &Trust,
) -> SimulationResult<()> {
    // Delayed registration: the listener is up, so references to earlier
    // boots are refused as stale, but this boot has published nothing yet.
    if buggify!() {
        assert_reachable!("rpc qual server delayed its registrations after a boot");
        let _ = ctx
            .time()
            .sleep(Duration::from_millis(ctx.random().random_range(1..800)))
            .await;
    }
    let Registered {
        base,
        private_ref,
        work,
        mut scans,
        mut private,
        mut directory,
        mut recruiter,
        mut forwarder,
    } = register(rpc, recorder)?;
    tracing::info!(line = %format!("{}#{} ready", recorder.me, recorder.boot), "rpc_qual_event");
    let streams = StreamLedger::of(ctx.state());
    let code = instance_code(&recorder.me, recorder.boot);
    let mut instances = Instances {
        streams: BTreeMap::from([(0, work)]),
        cursor: 0,
    };
    let local = local_caller(ctx, rpc, &private_ref, trust).fuse();
    futures::pin_mut!(local);
    let mut pending: FuturesUnordered<Pending<'_>> = FuturesUnordered::new();
    loop {
        moonpool_sim::select! {
            () = &mut local => {}
            Some(IncomingRequest { reply, .. }) = directory.next() => {
                let _ = reply.send(&base);
            }
            Some(IncomingRequest { request, reply }) = recruiter.next() => {
                if let Ok(publication) = recruit(rpc, recorder, &mut instances, request.configuration, &base) {
                    let _ = reply.send(&publication);
                }
            }
            Some(IncomingRequest { request, reply }) = forwarder.next() => {
                pending.push(Box::pin(async move {
                    let relayed = forward(rpc, request).await;
                    let _ = reply.send(&relayed);
                }));
            }
            (configuration, IncomingRequest { request, reply }) = instances.next() => {
                let done = recorder.run(&request, configuration);
                // A balanced job names no instance; this server is slow on
                // some of them, so hedges and late losers happen.
                let hold = if request.server.is_empty() && ctx.random().random_bool(0.2) {
                    ctx.random().random_range(100..600)
                } else {
                    request.hold_ms
                };
                if reply.expects_reply() {
                    pending.push(Box::pin(hold_and_reply(ctx, reply, hold, done)));
                }
            }
            Some(incoming) = scans.next() => {
                pending.push(Box::pin(produce(incoming, &streams, code, ctx)));
            }
            Some(incoming) = private.next() => {
                pending.push(private_request(ctx, trust, incoming));
            }
            Some(()) = pending.next(), if !pending.is_empty() => {}
        }
    }
}

/// Calls this runtime's own private endpoint with drawn credentials until
/// the faults stop: local delivery passes the same check as remote
/// delivery, judged by the same oracle.
async fn local_caller(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    private: &ServiceRef<PrivateEcho>,
    trust: &Trust,
) {
    let caller = Caller {
        ledger: AuthLedger::of(ctx.state()),
        trust: trust.clone(),
        route: Route::Local,
    };
    while !ctx.state().contains(SCRIPT_DONE_KEY) {
        let gap = Duration::from_millis(ctx.random().random_range(300..1500));
        if pause(ctx, gap).await.is_err() {
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
        if let Err(error) = &outcome
            && matches!(
                error.reason(),
                moonpool_rpc::ErrorReason::Unauthenticated(_)
            )
        {
            assert_sometimes!(true, "rpc qual local call denied by the same policy");
        }
        let _ = caller.judge(id, &outcome.map(Some));
    }
}

/// Register a fresh `Work` instance for `configuration` (or return the
/// existing one), recording its publication before it is advertised.
fn recruit(
    rpc: &RpcHandle<SimProviders>,
    recorder: &Recorder,
    instances: &mut Instances,
    configuration: u64,
    base: &Publication,
) -> Result<Publication, moonpool_rpc::RpcError> {
    if configuration == 0 {
        return Ok(base.clone());
    }
    let publication = |work: Vec<u8>| Publication {
        server: recorder.me.clone(),
        boot: recorder.boot,
        configuration,
        work,
        scan: Vec::new(),
        private: Vec::new(),
    };
    if let Some(stream) = instances.streams.get(&configuration) {
        return Ok(publication(stream.service_ref().to_bytes()));
    }
    let (work_ref, stream) = rpc.register::<Work>(AccessClass::Public)?;
    recorder.ledger.publish(
        work_ref.to_bytes(),
        Instance {
            process: recorder.me.clone(),
            boot: recorder.boot,
            configuration,
        },
    );
    instances.streams.insert(configuration, stream);
    Ok(publication(work_ref.to_bytes()))
}

/// Call the callback a request carries, through this server's own runtime:
/// the reference arrived as data from a third process.
async fn forward(rpc: &RpcHandle<SimProviders>, request: Forward) -> Relayed {
    let mut relayed = Relayed::default();
    let Some(callback) = request.callback else {
        relayed.failures.push(format!("{}:missing", request.id));
        return relayed;
    };
    let job = Job {
        id: request.id,
        server: callback.server.clone(),
        boot: callback.boot,
        configuration: callback.configuration,
        hold_ms: 0,
    };
    let Ok(target) = ServiceRef::<Callback>::from_bytes(&callback.work) else {
        relayed.failures.push(format!("{}:undecodable", request.id));
        return relayed;
    };
    let outcome = target
        .bind(rpc)
        .try_get_reply_within(&job, CALL_TIMEOUT)
        .await;
    match outcome {
        Ok(done) => relayed.answers.push(done),
        Err(error) => relayed.failures.push(format!(
            "{}:{:?}/{:?}",
            request.id,
            error.reason(),
            error.execution()
        )),
    }
    relayed
}
