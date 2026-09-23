//! The runtime: owned driver, weak handles, registry, peers, pending calls
//! and the failure monitor.
//!
//! # Ownership
//!
//! [`RpcDriver`] owns everything: the listener, every connection's I/O, the
//! endpoint registry, the peers and the pending calls (through the one
//! strong reference to the shared state). [`RpcHandle`],
//! [`ServiceClient`](crate::ServiceClient), [`RequestStream`](crate::RequestStream),
//! [`FailureMonitor`](crate::FailureMonitor) and reply handles only hold
//! weak references. Driving is the caller's job: poll [`RpcDriver::run`]
//! alongside the application (inside the process body in a simulation, so
//! a crash that aborts the process drops the whole tree). No task is
//! spawned: the accept loop and each connection are child futures the
//! driver polls, so dropping the driver cancels them before their next
//! poll, closes every socket, ends every
//! [`RequestStream`](crate::RequestStream), completes every pending call
//! with [`ErrorReason::Shutdown`](crate::ErrorReason::Shutdown) and releases
//! every failure-monitor waiter.
//!
//! # Sessions and peers
//!
//! A connection is upgraded ([`Connector`] / [`Acceptor`], [`Plaintext`](upgrade::Plaintext) by
//! default) before any frame is exchanged, then both sides send a `Hello`.
//! Requests are written only after the peer's `Hello` was accepted, so a
//! session refused at the handshake never carried a request and its calls
//! fail as [`Execution::NotAdmitted`](crate::Execution::NotAdmitted).
//!
//! Calls to a remote address ride that address's **selected connection**
//! (see [`peer`]). It is dialed on demand, re-dialed with a jittered
//! backoff after it ends, pinged while it lives and closed once idle. An
//! accepted session whose `Hello` names a listen address (verified per
//! [`InboundSharing`](crate::InboundSharing)) becomes the selected
//! connection to that address when none exists, so two runtimes calling
//! each other share one connection; when both dial at once, the runtime
//! with the larger canonical address keeps the connection it dialed and
//! the other adopts it and closes its own (`FoundationDB`'s
//! `Peer::onIncomingConnection`).
//!
//! # Delivery
//!
//! An at-most-once attempt is bound to the connection it was queued on:
//! when that connection ends, the attempt fails (`NotAdmitted` if its frame
//! never started to leave, `MaybeExecuted` otherwise). A reliable call
//! keeps its request in memory while its caller waits, and each time its
//! connection ends it is queued again on the next one, so the server may
//! execute it more than once. Unsent bytes belong to a connection;
//! retained requests belong to the call. Nothing is retained past the
//! caller: cancellation, a reply, a terminal rejection or shutdown release
//! it, without retracting bytes already written.

mod calls;
pub(crate) mod connection;
mod driver;
mod handle;
pub(crate) mod peer;
mod sessions;
mod shutdown;
mod streams;
pub mod upgrade;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};

use futures::channel::{mpsc, oneshot};
use moonpool_core::{NetworkProvider, Providers, RandomProvider};

use self::connection::{Connection, Direction, QueueLimits};
use self::peer::Peer;
use self::upgrade::{Acceptor, Connector, PeerContext};
use crate::call::receiver::{EndpointOwner, Inbox, RequestStream, endpoint_pair};
use crate::call::reply::{Outstanding, ReplyContext, ReplyRoute};
use crate::codec::CodecId;
use crate::config::RpcConfig;
use crate::endpoint::registry::{Registry, RegistryError};
use crate::endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation, WellKnownId};
use crate::error::{CallIdentity, ErrorReason, RpcError};
use crate::failure::watch::Watch;
use crate::failure::{MonitorState, PermanentFailure};
use crate::interface::{InterfaceId, RpcInterface, ServiceGroup, ServiceRef};
use crate::protocol::{
    MethodId, PROTOCOL_MAGIC, RpcMethod, SchemaVersion, WireError, WireMessage, encode_frame,
    encode_message,
};
use crate::security::{AccessRequest, CredentialSource, Denial, RequestOrigin, audit_denial};
use crate::stats::{Counters, RpcStats};
use crate::stream::consumer::ConsumerCore;

pub use self::driver::{RpcDriver, is_transient_accept_error};
pub use self::handle::RpcHandle;
pub use self::shutdown::ShutdownReport;

type Stream<P> = <<P as Providers>::Network as NetworkProvider>::TcpStream;
type Listener<P> = <<P as Providers>::Network as NetworkProvider>::TcpListener;

/// A session upgrade usable for both directions of provider `P`'s streams.
///
/// Blanket-implemented for every type that is both a [`Connector`] and an
/// [`Acceptor`] of `P`'s TCP stream.
pub trait SessionUpgrade<P: Providers>: Connector<Stream<P>> + Acceptor<Stream<P>> {}

impl<P: Providers, U> SessionUpgrade<P> for U where U: Connector<Stream<P>> + Acceptor<Stream<P>> {}

/// Work handed to the driver by handles and the accept loop.
enum Command<P: Providers> {
    /// Wait out the peer's backoff, then connect, upgrade and drive an
    /// outbound connection.
    Connect(Arc<Connection>, SocketAddr),
    /// Upgrade and drive an accepted connection.
    Accepted(Arc<Connection>, Stream<P>),
    /// A graceful shutdown began: stop accepting.
    StopAccepting,
}

/// Where a pending call's reply may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    Connection(u64),
}

/// A reply body and the codec that produced it.
pub(crate) type ReplyBytes = (CodecId, Vec<u8>);

/// How a call is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// One attempt, never retransmitted.
    AtMostOnce,
    /// Retransmitted on every new connection while the caller waits.
    Reliable,
}

struct PendingCall {
    origin: Origin,
    endpoint: Endpoint,
    identity: CallIdentity,
    /// The current attempt's frame began to leave this process.
    transmitted: bool,
    /// An earlier attempt of this (reliable) call began to leave.
    earlier_transmitted: bool,
    /// The request body, kept for retransmission (reliable calls only).
    retained: Option<Vec<u8>>,
    /// Where each attempt's credential comes from: asked again when a
    /// reliable call is retransmitted, so it never travels with a stale or
    /// missing credential.
    credentials: Option<Arc<dyn CredentialSource>>,
    completion: Completion,
}

impl PendingCall {
    /// Whether any attempt of this call may have reached a handler.
    fn may_have_executed(&self) -> bool {
        self.transmitted || self.earlier_transmitted
    }

    /// The consumer, for a call that opened a reply stream.
    fn stream(&self) -> Option<&Arc<ConsumerCore>> {
        match &self.completion {
            Completion::Reply(_) => None,
            Completion::Stream(stream) => Some(&stream.0),
        }
    }
}

/// How a pending call completes.
enum Completion {
    /// One reply, through a one-shot channel.
    Reply(oneshot::Sender<Result<ReplyBytes, RpcError>>),
    /// A reply stream: the call stays pending while items arrive, and its
    /// outcome here is only a failure before the stream's own end.
    Stream(StreamCompletion),
}

impl Completion {
    /// Complete with `result`.
    fn finish(self, result: Result<ReplyBytes, RpcError>) {
        match self {
            Self::Reply(sender) => {
                let _ = sender.send(result);
            }
            Self::Stream(stream) => stream.0.terminate(result.map_or_else(
                |error| error,
                |_| {
                    RpcError::new(
                        ErrorReason::StreamProtocol("a single reply answered a stream".into()),
                        crate::error::Execution::Executed,
                    )
                },
            )),
        }
    }
}

/// A pending stream's consumer. Dropped with its runtime (or its call) it
/// ends the stream as shut down, so no consumer waits forever; when the
/// stream already ended that is a no-op.
struct StreamCompletion(Arc<ConsumerCore>);

impl Drop for StreamCompletion {
    fn drop(&mut self) {
        self.0.terminate(RpcError::new(
            ErrorReason::Shutdown,
            crate::error::Execution::MaybeExecuted,
        ));
    }
}

/// One registry entry: a single-method endpoint, or a group whose methods
/// are told apart by their explicit ids.
struct Registration {
    methods: BTreeMap<MethodId, Arc<dyn Inbox>>,
    /// A group (from `register_group`) rather than one method's endpoint.
    grouped: bool,
    /// The group's interface and version; zero for a single-method
    /// endpoint.
    interface: (InterfaceId, SchemaVersion),
    /// Who may call it: enforced in admission.
    access: AccessClass,
}

impl Registration {
    fn single(inbox: Arc<dyn Inbox>, access: AccessClass) -> Self {
        let (method, _, _) = inbox.identity();
        Self {
            methods: BTreeMap::from([(method, inbox)]),
            grouped: false,
            interface: (InterfaceId::new(0), SchemaVersion::new(0)),
            access,
        }
    }

    /// The inbox serving `method` of `interface`, or the rejection that
    /// says why none does. The interface is checked first: a reference to
    /// another interface never reaches a method that happens to share an
    /// id.
    fn route(
        &self,
        interface: (InterfaceId, SchemaVersion),
        method: MethodId,
    ) -> Result<Arc<dyn Inbox>, WireError> {
        if interface != self.interface {
            return Err(WireError::InterfaceMismatch {
                registered: self.interface,
            });
        }
        if let Some(inbox) = self.methods.get(&method) {
            return Ok(Arc::clone(inbox));
        }
        match (self.grouped, self.methods.keys().next()) {
            (false, Some(registered)) => Err(WireError::MethodMismatch {
                registered: *registered,
            }),
            _ => Err(WireError::MethodNotFound),
        }
    }
}

struct State {
    registry: Registry<Registration>,
    well_known: BTreeMap<WellKnownId, Registration>,
    pending: BTreeMap<u64, PendingCall>,
    next_call_id: u64,
    /// One relationship per canonical remote address.
    peers: BTreeMap<SocketAddr, Peer>,
    /// Every open connection by id.
    connections: BTreeMap<u64, Arc<Connection>>,
    next_connection_id: u64,
    next_ping: u64,
    monitor: MonitorState,
}

/// The runtime's shared state; the driver holds the only strong reference.
pub(crate) struct Shared<P: Providers> {
    providers: P,
    config: RpcConfig,
    incarnation: Incarnation,
    address: Option<SocketAddr>,
    state: Mutex<State>,
    commands: mpsc::UnboundedSender<Command<P>>,
    counters: Arc<Counters>,
    /// Published after every failure-monitor change; closed on drop.
    watch: Arc<Watch>,
    alive: Arc<()>,
    /// How accepted sessions may be shared: the configured mode, or
    /// `Disabled` when the session upgrade authenticates servers.
    inbound_sharing: crate::config::InboundSharing,
    /// [`RUNNING`], [`DRAINING`] or [`TERMINATED`] (graceful shutdown).
    lifecycle: std::sync::atomic::AtomicU8,
    this: Weak<Shared<P>>,
}

/// The runtime admits and starts work.
const RUNNING: u8 = 0;
/// A graceful shutdown began: nothing new is admitted, started, dialed or
/// accepted; admitted work may finish.
const DRAINING: u8 = 1;
/// The drain ended: every remaining call was failed and every connection
/// closed.
const TERMINATED: u8 = 2;

impl<P: Providers> Drop for Shared<P> {
    fn drop(&mut self) {
        // Release every failure-monitor waiter: the runtime is gone.
        self.watch.close();
    }
}

impl<P: Providers> Shared<P> {
    fn new(
        providers: P,
        config: RpcConfig,
        address: Option<SocketAddr>,
        commands: mpsc::UnboundedSender<Command<P>>,
        authenticates_server: bool,
    ) -> Arc<Self> {
        let inbound_sharing = if authenticates_server {
            if config.peer.share_inbound_sessions != crate::config::InboundSharing::Disabled {
                tracing::debug!(
                    "rpc inbound session sharing off: the session upgrade authenticates servers"
                );
            }
            crate::config::InboundSharing::Disabled
        } else {
            config.peer.share_inbound_sessions
        };
        // Protocol identifiers come from the provider's random source (seeded
        // in simulation, OS entropy in production) unless the caller supplies
        // one. Never an authentication.
        let incarnation = config
            .incarnation
            .unwrap_or_else(|| Incarnation::from_raw(providers.random().random::<u128>()));
        Arc::new_cyclic(|this| Self {
            providers,
            state: Mutex::new(State {
                registry: Registry::new(config.max_endpoints),
                well_known: BTreeMap::new(),
                pending: BTreeMap::new(),
                next_call_id: 0,
                peers: BTreeMap::new(),
                connections: BTreeMap::new(),
                next_connection_id: 0,
                next_ping: 0,
                monitor: MonitorState::new(
                    config.peer.max_failed_endpoints,
                    config.peer.max_tracked_addresses,
                ),
            }),
            config,
            incarnation,
            address,
            commands,
            counters: Arc::new(Counters::default()),
            watch: Arc::new(Watch::default()),
            alive: Arc::new(()),
            inbound_sharing,
            lifecycle: std::sync::atomic::AtomicU8::new(RUNNING),
            this: this.clone(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    pub(crate) fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    pub(crate) fn time(&self) -> &P::Time {
        self.providers.time()
    }

    pub(crate) fn config(&self) -> &RpcConfig {
        &self.config
    }

    pub(crate) fn random(&self) -> &P::Random {
        self.providers.random()
    }

    fn now(&self) -> std::time::Duration {
        moonpool_core::TimeProvider::now(self.time())
    }

    pub(crate) fn watch(&self) -> &Arc<Watch> {
        &self.watch
    }

    pub(crate) fn read_monitor<T>(&self, read: impl FnOnce(&MonitorState) -> T) -> T {
        read(&self.lock().monitor)
    }

    /// A failure-monitor watcher of `address` starts or stops.
    pub(crate) fn reference(&self, address: SocketAddr, add: bool) {
        self.lock().monitor.reference(address, add);
    }

    /// Whether a graceful shutdown began: nothing new may start.
    pub(crate) fn is_closing(&self) -> bool {
        self.lifecycle.load(std::sync::atomic::Ordering::Acquire) != RUNNING
    }

    /// The refusal of new work once a graceful shutdown began.
    fn refuse_if_closing(&self) -> Result<(), RpcError> {
        if self.is_closing() {
            Err(RpcError::not_admitted(ErrorReason::Shutdown))
        } else {
            Ok(())
        }
    }

    fn hello(&self) -> Vec<u8> {
        let (min_version, max_version) = self.config.advertised_versions();
        let payload = encode_message(&WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            min_version,
            max_version,
            incarnation: self.incarnation,
            features: 0,
            max_frame_bytes: self.config.max_frame_bytes,
            // Announced only when this runtime shares sessions: a peer then
            // knows the tie-break applies on both sides.
            listen: self
                .address
                .filter(|_| self.inbound_sharing != crate::config::InboundSharing::Disabled),
        });
        // A handshake is a few dozen bytes and exempt from the configured
        // frame limit, which only bounds what peers may send us.
        encode_frame(&payload, u32::MAX).unwrap_or_default()
    }

    fn context(
        &self,
        route: ReplyRoute,
        peer: Option<PeerContext>,
        outstanding: Option<Outstanding>,
    ) -> ReplyContext {
        ReplyContext {
            route,
            counters: Arc::clone(&self.counters),
            max_frame_bytes: self.config.max_frame_bytes,
            peer,
            principal: None,
            outstanding,
            stream: None,
        }
    }

    fn register<M: RpcMethod>(
        &self,
        access: AccessClass,
        well_known: Option<WellKnownId>,
    ) -> Result<(ServiceRef<M>, RequestStream<M>), RpcError> {
        self.refuse_if_closing()?;
        let address = self
            .address
            .ok_or(RpcError::not_admitted(ErrorReason::NotListening))?;
        let (inbox, receiver) = endpoint_pair::<M>(self.queue_capacity());
        let token = self.insert(Registration::single(inbox, access), well_known)?;
        let endpoint = Endpoint::new(address, self.incarnation, token);
        let owner: Weak<dyn EndpointOwner> = self.this.clone();
        let stream = receiver.bind(ServiceRef::new(endpoint, access), owner, false);
        tracing::debug!(method = M::NAME, %endpoint, ?access, "rpc endpoint registered");
        Ok((stream.service_ref().clone(), stream))
    }

    fn register_group<I: RpcInterface>(
        &self,
        access: AccessClass,
    ) -> Result<ServiceGroup<I>, RpcError> {
        self.refuse_if_closing()?;
        let address = self
            .address
            .ok_or(RpcError::not_admitted(ErrorReason::NotListening))?;
        let registration = Registration {
            methods: BTreeMap::new(),
            grouped: true,
            interface: (I::INTERFACE, I::VERSION),
            access,
        };
        let token = self.insert(registration, None)?;
        let endpoint = Endpoint::new(address, self.incarnation, token);
        tracing::debug!(interface = I::NAME, %endpoint, ?access, "rpc endpoint group registered");
        Ok(ServiceGroup::new(endpoint, access, self.this.clone()))
    }

    fn insert(
        &self,
        registration: Registration,
        well_known: Option<WellKnownId>,
    ) -> Result<EndpointToken, RpcError> {
        let mut state = self.lock();
        match well_known {
            Some(id) => {
                if state.well_known.contains_key(&id) {
                    return Err(RpcError::not_admitted(ErrorReason::AlreadyRegistered));
                }
                state.well_known.insert(id, registration);
                Ok(EndpointToken::well_known(id))
            }
            None => state
                .registry
                .insert(registration)
                .map_err(|RegistryError::Full| RpcError::not_admitted(ErrorReason::Overloaded)),
        }
    }

    /// Validate and hand one request to its receiver. Local and remote
    /// admissions both come through here. Every rejection is delivered
    /// along the context's route before returning.
    ///
    /// Order: shutdown, identity (incarnation, token), contract (interface,
    /// method, schema, codec, streaming), **security** (credential and
    /// access policy), then capacity; only then is the body decoded and
    /// handed to the endpoint. A request that fails any check never reaches
    /// a handler or a queue.
    fn admit(&self, request: &Admission<'_>, mut context: ReplyContext) {
        let token = request.token;
        if request.local && request.metadata.is_some_and(|section| !section.is_empty()) {
            // A local caller's credential: it reached admission without
            // crossing a session.
            Counters::bump(&self.counters.credentials_attached);
        }
        if self.is_closing() {
            Counters::bump(&self.counters.shutdown_refusals);
            Counters::bump(&self.counters.requests_rejected);
            tracing::debug!(%token, "rpc request refused: shutting down");
            context.reject(WireError::ShuttingDown);
            return;
        }
        // A well-known endpoint answers in every incarnation; a dynamic one
        // only in the incarnation that registered it.
        let wrong_incarnation = !token.is_well_known() && request.incarnation != self.incarnation;
        let inbox = if wrong_incarnation {
            Err(WireError::StaleIncarnation)
        } else {
            let state = self.lock();
            match token.well_known_id() {
                Some(id) => state.well_known.get(&id),
                None => state.registry.get(token),
            }
            .map_or(Err(WireError::EndpointNotFound), |registration| {
                registration
                    .route(request.identity.interface, request.identity.method)
                    .map(|inbox| (inbox, registration.access))
            })
        };
        // Checked in order, all before a single body byte is decoded.
        let rejection = match inbox {
            Err(rejection) => rejection,
            Ok((inbox, access)) => {
                let (method, schema, codec) = inbox.identity();
                if method != request.identity.method {
                    WireError::MethodMismatch { registered: method }
                } else if schema != request.identity.schema {
                    WireError::SchemaMismatch { registered: schema }
                } else if codec != request.identity.codec {
                    WireError::CodecMismatch { registered: codec }
                } else if inbox.streaming() != request.stream_window.is_some() {
                    WireError::StreamingMismatch {
                        endpoint_streams: inbox.streaming(),
                    }
                } else if let Err(denial) = self.authorize(request, access, &mut context) {
                    match denial {
                        Denial::Unauthenticated(reason) => WireError::Unauthenticated { reason },
                        Denial::PermissionDenied => WireError::PermissionDenied,
                    }
                } else if context
                    .outstanding
                    .as_ref()
                    .is_some_and(|owed| owed.over(&self.config.limits))
                {
                    // Every admitted request reserves the room of its reply:
                    // beyond the in-flight budget, push back before any
                    // handler sees it.
                    Counters::bump(&self.counters.overload_refusals);
                    WireError::Overloaded
                } else if let Some(window) = request.stream_window
                    && let Err(refusal) = self.open_stream(&mut context, window)
                {
                    refusal
                } else {
                    match inbox.deliver(request.body, context) {
                        Ok(()) => Counters::bump(&self.counters.requests_admitted),
                        Err(error) => {
                            Counters::bump(&self.counters.requests_rejected);
                            tracing::debug!(?error, %token, "rpc request refused");
                        }
                    }
                    return;
                }
            }
        };
        Counters::bump(&self.counters.requests_rejected);
        tracing::debug!(error = ?rejection, %token, "rpc request refused");
        context.reject(rejection);
    }

    /// The security check of one request whose endpoint and contract
    /// matched: verify its credential and apply the access policy. On
    /// success the verified principal goes into the reply context.
    fn authorize(
        &self,
        request: &Admission<'_>,
        access: AccessClass,
        context: &mut ReplyContext,
    ) -> Result<(), Denial> {
        let security = &self.config.security;
        // The admission site says where the request came from; a remote
        // request whose session context is missing is judged as an
        // unencrypted, unauthenticated peer (fail closed).
        let unknown = PeerContext::new("unknown");
        let origin = if request.local {
            RequestOrigin::Local
        } else {
            RequestOrigin::Remote(context.peer.as_ref().unwrap_or(&unknown))
        };
        let check = AccessRequest {
            origin,
            access,
            token: request.token,
            interface: request.identity.interface,
            method: request.identity.method,
            now: if security.is_trusted_network() {
                None
            } else {
                security.now_utc()
            },
        };
        match security.decide(&check, request.metadata) {
            Ok(principal) => {
                if principal.is_some() {
                    Counters::bump(&self.counters.requests_authenticated);
                }
                context.principal = principal.map(Arc::new);
                Ok(())
            }
            Err(denial) => {
                self.counters.count_denial(denial);
                audit_denial(&check, denial);
                Err(denial)
            }
        }
    }

    /// The per-endpoint queue budget: requests and bytes.
    fn queue_capacity(&self) -> (usize, u64) {
        (
            self.config.endpoint_queue_capacity,
            self.config.limits.endpoint_queue_bytes,
        )
    }

    /// The live selected connection to `address`, opening one if needed.
    fn peer_connection(
        &self,
        state: &mut State,
        address: SocketAddr,
    ) -> Result<Arc<Connection>, RpcError> {
        // Draining: no new work, and no new session or re-dial for old work.
        self.refuse_if_closing()?;
        let now = self.now();
        if let Some(peer) = state.peers.get(&address)
            && peer.dialing_stalled(now, self.config.peer.always_accept_after)
            && let Some(candidate) = peer.usable_candidate()
        {
            // Our own dials keep failing while the peer's session to us
            // works: use it (`ALWAYS_ACCEPT_DELAY`).
            let replaced = peer
                .live()
                .filter(|current| !current.is_established())
                .cloned();
            if peer.live().is_none() || replaced.is_some() {
                Counters::bump(&self.counters.accepted_over_stalled_dial);
                self.adopt(state, address, &candidate, replaced, now);
                return Ok(candidate);
            }
        }
        if let Some(existing) = state.peers.get(&address).and_then(Peer::live) {
            return Ok(Arc::clone(existing));
        }
        if state.connections.len() >= self.config.max_connections {
            Counters::bump(&self.counters.connections_rejected);
            return Err(RpcError::not_admitted(ErrorReason::Overloaded));
        }
        if !state.peers.contains_key(&address)
            && state.peers.len() >= self.config.peer.max_tracked_addresses
        {
            state.peers.retain(|_, peer| !peer.is_forgettable(now));
        }
        let connection = self.new_connection(state, address.to_string(), Direction::Outbound);
        connection.select_for(address);
        state
            .peers
            .entry(address)
            .or_insert_with(|| Peer::new(&self.config.peer))
            .current = Some(Arc::clone(&connection));
        if self
            .commands
            .unbounded_send(Command::Connect(Arc::clone(&connection), address))
            .is_err()
        {
            state.connections.remove(&connection.id());
            let _ = connection.close(connection::CloseReason::Local);
            return Err(RpcError::not_admitted(ErrorReason::Shutdown));
        }
        Ok(connection)
    }

    fn new_connection(
        &self,
        state: &mut State,
        peer: String,
        direction: Direction,
    ) -> Arc<Connection> {
        let id = state.next_connection_id;
        state.next_connection_id += 1;
        let connection = Arc::new(Connection::new(
            id,
            peer,
            direction,
            self.hello(),
            QueueLimits {
                requests: self.config.max_queued_requests,
                control: self.config.reserved_control_frames,
                bytes: self.config.limits.max_queued_bytes_per_connection,
                runtime_bytes: self.config.limits.max_queued_bytes,
            },
            self.now(),
            &self.counters,
        ));
        state.connections.insert(id, Arc::clone(&connection));
        connection
    }

    fn accept(&self, stream: Stream<P>, peer: String) {
        if self.is_closing() {
            return;
        }
        if !self.config.security.allow_list().allows_peer(&peer) {
            Counters::bump(&self.counters.connections_refused_by_policy);
            tracing::warn!(
                target: "moonpool_rpc::audit",
                from = %peer,
                "rpc_connection_refused"
            );
            return;
        }
        let mut state = self.lock();
        if state.connections.len() >= self.config.max_connections {
            drop(state);
            Counters::bump(&self.counters.connections_rejected);
            tracing::warn!(%peer, "rpc connection budget exhausted, refusing inbound connection");
            return;
        }
        let connection = self.new_connection(&mut state, peer, Direction::Inbound);
        drop(state);
        // The receiver lives in the driver that runs this accept loop.
        let _ = self
            .commands
            .unbounded_send(Command::Accepted(connection, stream));
    }

    fn stats(&self) -> RpcStats {
        let state = self.lock();
        let endpoints = state.registry.live() + state.well_known.len();
        let retained = state
            .pending
            .values()
            .filter(|call| call.retained.is_some())
            .count();
        let consuming = state
            .pending
            .values()
            .filter(|call| call.stream().is_some())
            .count();
        let mut snapshot = self
            .counters
            .snapshot(endpoints, state.pending.len(), retained);
        snapshot.peers = state.peers.len();
        snapshot.streams_consuming = consuming;
        snapshot
    }
}

impl<P: Providers> EndpointOwner for Shared<P> {
    fn unregister(&self, token: EndpointToken, method: Option<MethodId>) {
        let mut state = self.lock();
        let removed = match method {
            None => match token.well_known_id() {
                Some(id) => state.well_known.remove(&id),
                None => state.registry.remove(token),
            }
            .map(|registration| registration.methods),
            Some(method) => state
                .registry
                .get_mut(token)
                .and_then(|registration| registration.methods.remove(&method))
                .map(|inbox| BTreeMap::from([(method, inbox)])),
        };
        drop(state);
        // Dropped outside the lock: closing an inbox breaks queued promises.
        drop(removed);
        tracing::debug!(%token, ?method, "rpc endpoint destroyed");
    }

    fn attach(
        &self,
        token: EndpointToken,
        method: MethodId,
        inbox: Arc<dyn Inbox>,
    ) -> Result<(), ErrorReason> {
        let mut state = self.lock();
        let registration = state
            .registry
            .get_mut(token)
            .filter(|registration| registration.grouped)
            .ok_or(ErrorReason::EndpointNotFound)?;
        if registration.methods.contains_key(&method) {
            return Err(ErrorReason::AlreadyRegistered);
        }
        registration.methods.insert(method, inbox);
        Ok(())
    }

    fn queue_capacity(&self) -> (usize, u64) {
        Shared::queue_capacity(self)
    }
}

/// One request presented for admission, from either route.
struct Admission<'a> {
    /// A caller in this runtime (as opposed to one over a session): set by
    /// the admission site itself, never inferred.
    local: bool,
    incarnation: Incarnation,
    token: EndpointToken,
    identity: CallIdentity,
    /// The caller's credit window, for a request that opens a stream.
    stream_window: Option<u64>,
    /// The request's credential section, or `None` when its session cannot
    /// carry credentials (protocol version 1).
    metadata: Option<&'a [u8]>,
    body: &'a [u8],
}

/// The failure a server rejection proves about a dynamic reference, if any.
fn permanent_failure(error: WireError) -> Option<PermanentFailure> {
    match error {
        WireError::EndpointNotFound => Some(PermanentFailure::NotFound),
        WireError::StaleIncarnation => Some(PermanentFailure::StaleIncarnation),
        _ => None,
    }
}

/// The terminal error a remembered permanent failure produces.
fn permanent_reason(failure: PermanentFailure) -> ErrorReason {
    match failure {
        PermanentFailure::NotFound => ErrorReason::EndpointNotFound,
        PermanentFailure::StaleIncarnation => ErrorReason::StaleIncarnation,
    }
}
