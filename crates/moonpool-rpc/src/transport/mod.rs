//! The runtime: owned driver, weak handles, registry, pending calls and
//! connection management.
//!
//! # Ownership
//!
//! [`RpcDriver`] owns everything: the listener, every connection's I/O, the
//! endpoint registry and the pending calls (through the one strong reference
//! to the shared state). [`RpcHandle`], [`ServiceClient`](crate::ServiceClient),
//! [`RequestStream`](crate::RequestStream) and reply handles only hold weak
//! references. Driving is the caller's job: poll [`RpcDriver::run`]
//! alongside the application (inside the process body in a simulation, so a
//! crash that aborts the process drops the whole tree). No task is spawned:
//! the accept loop and each connection are child futures the driver polls,
//! so dropping the driver cancels them before their next poll, closes every
//! socket, ends every [`RequestStream`](crate::RequestStream), and completes
//! every pending call with [`ErrorReason::Shutdown`](crate::ErrorReason::Shutdown).
//!
//! # Sessions
//!
//! A connection is upgraded ([`Connector`] / [`Acceptor`], [`Plaintext`] by
//! default) before any frame is exchanged, then both sides send a `Hello`.
//! Requests are written only after the peer's `Hello` was accepted, so a
//! session refused at the handshake never carried a request and its calls
//! fail as [`Execution::NotAdmitted`](crate::Execution::NotAdmitted).

pub(crate) mod connection;
pub mod upgrade;

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::FuturesUnordered;
use futures::{AsyncReadExt, StreamExt};
use moonpool_core::{NetworkProvider, Providers, RandomProvider, TcpListenerTrait, TimeProvider};

use self::connection::{CloseReason, Connection, QueueRefusal, read_loop, write_loop};
use self::upgrade::{Acceptor, Connector, PeerContext, Plaintext};
use crate::ServiceRef;
use crate::call::client::{CallGuard, CallOwner, ServiceClient};
use crate::call::receiver::{EndpointOwner, Inbox, RequestStream, endpoint_pair};
use crate::call::reply::{LocalSink, ReplyContext, ReplyRoute};
use crate::codec::CodecId;
use crate::config::RpcConfig;
use crate::endpoint::registry::{Registry, RegistryError};
use crate::endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation};
use crate::error::{CallIdentity, ErrorReason, Execution, RpcError};
use crate::protocol::{
    MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION, RpcMethod, WireError, WireMessage,
    WireOutcome, decode_message, encode_frame, encode_message, negotiate, request_envelope_len,
};
use crate::stats::{Counters, ResourceProbe, RpcStats, TaskGuard};

type Stream<P> = <<P as Providers>::Network as NetworkProvider>::TcpStream;
type Listener<P> = <<P as Providers>::Network as NetworkProvider>::TcpListener;
type ChildFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A session upgrade usable for both directions of provider `P`'s streams.
///
/// Blanket-implemented for every type that is both a [`Connector`] and an
/// [`Acceptor`] of `P`'s TCP stream.
pub trait SessionUpgrade<P: Providers>: Connector<Stream<P>> + Acceptor<Stream<P>> {}

impl<P: Providers, U> SessionUpgrade<P> for U where U: Connector<Stream<P>> + Acceptor<Stream<P>> {}

/// Work handed to the driver by handles and the accept loop.
enum Command<P: Providers> {
    /// Connect, upgrade and drive an outbound connection.
    Connect(Arc<Connection>, SocketAddr),
    /// Upgrade and drive an accepted connection.
    Accepted(Arc<Connection>, Stream<P>),
}

/// Where a pending call's reply may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    Connection(u64),
}

/// A reply body and the codec that produced it.
pub(crate) type ReplyBytes = (CodecId, Vec<u8>);

struct PendingCall {
    origin: Origin,
    identity: CallIdentity,
    transmitted: bool,
    sender: oneshot::Sender<Result<ReplyBytes, RpcError>>,
}

struct Registration {
    inbox: Arc<dyn Inbox>,
    // Stored and carried now, enforced by the security package (#218).
    _access: AccessClass,
}

struct State {
    registry: Registry<Registration>,
    pending: BTreeMap<u64, PendingCall>,
    next_call_id: u64,
    /// The current outbound connection per address.
    outbound: BTreeMap<SocketAddr, Arc<Connection>>,
    /// Every open connection by id.
    connections: BTreeMap<u64, Arc<Connection>>,
    next_connection_id: u64,
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
    alive: Arc<()>,
    this: Weak<Shared<P>>,
}

impl<P: Providers> Shared<P> {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    pub(crate) fn time(&self) -> &P::Time {
        self.providers.time()
    }

    fn hello(&self) -> Vec<u8> {
        let payload = encode_message(&WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            incarnation: self.incarnation,
            features: 0,
        });
        // A handshake is a few dozen bytes and exempt from the configured
        // frame limit, which only bounds what peers may send us.
        encode_frame(&payload, u32::MAX).unwrap_or_default()
    }

    fn context(&self, route: ReplyRoute, peer: Option<PeerContext>) -> ReplyContext {
        ReplyContext {
            route,
            counters: Arc::clone(&self.counters),
            max_frame_bytes: self.config.max_frame_bytes,
            peer,
        }
    }

    fn register<M: RpcMethod>(
        &self,
        access: AccessClass,
    ) -> Result<(ServiceRef<M>, RequestStream<M>), RpcError> {
        let address = self
            .address
            .ok_or(RpcError::not_admitted(ErrorReason::NotListening))?;
        let (inbox, receiver) = endpoint_pair::<M>(self.config.endpoint_queue_capacity);
        let token = self
            .lock()
            .registry
            .insert(Registration {
                inbox,
                _access: access,
            })
            .map_err(|RegistryError::Full| RpcError::not_admitted(ErrorReason::Overloaded))?;
        let endpoint = Endpoint::new(address, self.incarnation, token);
        let owner: Weak<dyn EndpointOwner> = self.this.clone();
        let stream = receiver.bind(ServiceRef::new(endpoint, access), owner);
        tracing::debug!(method = M::NAME, %endpoint, ?access, "rpc endpoint registered");
        Ok((stream.service_ref().clone(), stream))
    }

    /// Validate and hand one request to its receiver. Local and remote
    /// admissions both come through here. Every rejection is delivered
    /// along `route` before returning.
    fn admit(&self, request: &Admission<'_>, route: ReplyRoute, peer: Option<PeerContext>) {
        let context = self.context(route, peer);
        let token = request.token;
        let inbox = if request.incarnation == self.incarnation {
            self.lock()
                .registry
                .get(token)
                .map(|registration| Arc::clone(&registration.inbox))
        } else {
            None
        };
        // Checked in order, all before a single body byte is decoded.
        let rejection = match inbox {
            _ if request.incarnation != self.incarnation => WireError::StaleIncarnation,
            None => WireError::EndpointNotFound,
            Some(inbox) => {
                let (method, schema, codec) = inbox.identity();
                if method != request.identity.method {
                    WireError::MethodMismatch { registered: method }
                } else if schema != request.identity.schema {
                    WireError::SchemaMismatch { registered: schema }
                } else if codec != request.identity.codec {
                    WireError::CodecMismatch { registered: codec }
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
        context.deliver(WireOutcome::Err(rejection));
    }

    /// Start one call attempt. The returned receiver completes exactly once.
    pub(crate) fn start_call(
        &self,
        endpoint: &Endpoint,
        identity: CallIdentity,
        body: Vec<u8>,
    ) -> Result<(oneshot::Receiver<Result<ReplyBytes, RpcError>>, CallGuard), RpcError> {
        let local = self.address == Some(endpoint.address());
        // Same frame limit on both routes: nothing oversized is admitted.
        let size = request_envelope_len(body.len()) as u64;
        if size > u64::from(self.config.max_frame_bytes) {
            return Err(RpcError::not_admitted(ErrorReason::FrameTooLarge {
                size,
                limit: self.config.max_frame_bytes,
            }));
        }
        let overloaded = || RpcError::not_admitted(ErrorReason::Overloaded);
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        if state.pending.len() >= self.config.max_pending_calls {
            return Err(overloaded());
        }
        let call_id = state.next_call_id;
        state.next_call_id = call_id.checked_add(1).ok_or_else(overloaded)?;
        let owner: Weak<dyn CallOwner> = self.this.clone();
        let guard = CallGuard::new(owner, call_id);

        if local {
            // Admission below is synchronous: once it returns, the request
            // either reached a receiver or was refused with an outcome.
            state.pending.insert(
                call_id,
                PendingCall {
                    origin: Origin::Local,
                    identity,
                    transmitted: true,
                    sender,
                },
            );
            drop(state);
            Counters::bump(&self.counters.calls_started);
            let sink: Weak<dyn LocalSink> = self.this.clone();
            // The same bytes, validation order, admission and frame limit as
            // the remote path; only the socket is skipped.
            self.admit(
                &Admission {
                    incarnation: endpoint.incarnation(),
                    token: endpoint.token(),
                    identity,
                    body: &body,
                },
                ReplyRoute::Local { sink, call_id },
                None,
            );
            return Ok((receiver, guard));
        }

        let frame = request_frame(
            call_id,
            endpoint,
            identity,
            body,
            self.config.max_frame_bytes,
        )?;
        let connection = self.connection_for(&mut state, endpoint.address())?;
        match connection.push_request(frame, call_id) {
            Ok(()) => {}
            Err(QueueRefusal::Full) => return Err(overloaded()),
            Err(QueueRefusal::Closed) => {
                return Err(RpcError::not_admitted(ErrorReason::Disconnected));
            }
        }
        state.pending.insert(
            call_id,
            PendingCall {
                origin: Origin::Connection(connection.id()),
                identity,
                transmitted: false,
                sender,
            },
        );
        drop(state);
        Counters::bump(&self.counters.calls_started);
        Ok((receiver, guard))
    }

    /// The live outbound connection to `address`, opening one if needed.
    fn connection_for(
        &self,
        state: &mut State,
        address: SocketAddr,
    ) -> Result<Arc<Connection>, RpcError> {
        if let Some(existing) = state.outbound.get(&address)
            && !existing.is_closed()
        {
            return Ok(Arc::clone(existing));
        }
        if state.connections.len() >= self.config.max_connections {
            Counters::bump(&self.counters.connections_rejected);
            return Err(RpcError::not_admitted(ErrorReason::Overloaded));
        }
        let connection = self.new_connection(state, address.to_string());
        state.outbound.insert(address, Arc::clone(&connection));
        self.commands
            .unbounded_send(Command::Connect(Arc::clone(&connection), address))
            .map_err(|_| RpcError::not_admitted(ErrorReason::Shutdown))?;
        Ok(connection)
    }

    fn new_connection(&self, state: &mut State, peer: String) -> Arc<Connection> {
        let id = state.next_connection_id;
        state.next_connection_id += 1;
        let connection = Arc::new(Connection::new(
            id,
            peer,
            self.hello(),
            self.config.max_queued_requests,
            self.config.reserved_control_frames,
            &self.counters,
        ));
        state.connections.insert(id, Arc::clone(&connection));
        connection
    }

    fn accept(&self, stream: Stream<P>, peer: String) {
        let mut state = self.lock();
        if state.connections.len() >= self.config.max_connections {
            drop(state);
            Counters::bump(&self.counters.connections_rejected);
            tracing::warn!(%peer, "rpc connection budget exhausted, refusing inbound connection");
            return;
        }
        let connection = self.new_connection(&mut state, peer);
        drop(state);
        // The receiver lives in the driver that runs this accept loop.
        let _ = self
            .commands
            .unbounded_send(Command::Accepted(connection, stream));
    }

    fn mark_transmitted(&self, call_id: u64) {
        if let Some(call) = self.lock().pending.get_mut(&call_id) {
            call.transmitted = true;
        }
    }

    /// Complete a pending call from `origin`.
    fn complete(&self, origin: Origin, call_id: u64, outcome: WireOutcome) {
        let mut state = self.lock();
        let Some(call) = state.pending.get(&call_id) else {
            drop(state);
            Counters::bump(&self.counters.late_replies);
            tracing::debug!(call_id, "rpc late reply discarded");
            return;
        };
        if call.origin != origin {
            drop(state);
            Counters::bump(&self.counters.misrouted_replies);
            tracing::warn!(
                call_id,
                ?origin,
                "rpc reply arrived on the wrong session, discarded"
            );
            return;
        }
        let Some(call) = state.pending.remove(&call_id) else {
            return;
        };
        drop(state);
        let result = match outcome {
            WireOutcome::Ok { codec, body } => Ok((codec, body)),
            WireOutcome::Err(error) => Err(RpcError::from_wire(error, call.identity)),
        };
        let _ = call.sender.send(result);
    }

    /// Handle one decoded frame from `connection`.
    fn on_message(
        &self,
        connection: &Arc<Connection>,
        message: WireMessage,
    ) -> Result<(), CloseReason> {
        let established = connection.is_established();
        match message {
            WireMessage::Hello {
                magic,
                min_version,
                max_version,
                incarnation,
                features,
            } => {
                if established {
                    return Err(CloseReason::Protocol("duplicate handshake".into()));
                }
                if magic != PROTOCOL_MAGIC {
                    return Err(CloseReason::Protocol(format!("bad magic {magic:#x}")));
                }
                let Some(version) = negotiate(
                    (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
                    (min_version, max_version),
                ) else {
                    return Err(CloseReason::Version(format!(
                        "peer speaks {min_version}..={max_version}, \
                         this build {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}"
                    )));
                };
                connection.establish();
                tracing::debug!(
                    peer = %connection.peer(),
                    %incarnation,
                    version,
                    features,
                    "rpc session established"
                );
                Ok(())
            }
            _ if !established => Err(CloseReason::Protocol("frame before handshake".into())),
            WireMessage::Request {
                call_id,
                incarnation,
                token,
                method,
                schema,
                codec,
                // Reserved for request credentials (#218): carried, never
                // interpreted or handed to user code by this version.
                metadata: _,
                body,
            } => {
                let route = ReplyRoute::Remote {
                    connection: Arc::downgrade(connection),
                    call_id,
                };
                self.admit(
                    &Admission {
                        incarnation,
                        token,
                        identity: CallIdentity {
                            method,
                            schema,
                            codec,
                        },
                        body: &body,
                    },
                    route,
                    connection.peer_context(),
                );
                Ok(())
            }
            WireMessage::Reply { call_id, outcome } => {
                self.complete(Origin::Connection(connection.id()), call_id, outcome);
                Ok(())
            }
        }
    }

    /// Tear down a finished connection and fail the calls it carried.
    fn close_connection(&self, connection: &Arc<Connection>, reason: &CloseReason) {
        let established = connection.is_established();
        connection.close();
        let mut state = self.lock();
        state.connections.remove(&connection.id());
        let address = connection.peer().parse::<SocketAddr>().ok();
        if let Some(address) = address
            && state
                .outbound
                .get(&address)
                .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            state.outbound.remove(&address);
        }
        let origin = Origin::Connection(connection.id());
        let failed: Vec<u64> = state
            .pending
            .iter()
            .filter(|(_, call)| call.origin == origin)
            .map(|(call_id, _)| *call_id)
            .collect();
        let calls: Vec<PendingCall> = failed
            .iter()
            .filter_map(|call_id| state.pending.remove(call_id))
            .collect();
        drop(state);
        match reason {
            CloseReason::Protocol(detail)
            | CloseReason::Checksum(detail)
            | CloseReason::Version(detail) => {
                Counters::bump(&self.counters.protocol_violations);
                if matches!(reason, CloseReason::Checksum(_)) {
                    Counters::bump(&self.counters.checksum_failures);
                }
                if matches!(reason, CloseReason::Version(_)) {
                    Counters::bump(&self.counters.version_rejections);
                }
                tracing::warn!(peer = %connection.peer(), %detail, "rpc protocol violation, connection closed");
            }
            _ => tracing::debug!(peer = %connection.peer(), ?reason, "rpc connection closed"),
        }
        for call in calls {
            let error = if let CloseReason::ConnectFailed(detail) = reason {
                RpcError::not_admitted(ErrorReason::ConnectFailed(detail.clone()))
            } else if !established {
                // Requests wait for the handshake: none of these left.
                RpcError::not_admitted(ErrorReason::ConnectFailed(format!(
                    "session closed during handshake: {reason:?}"
                )))
            } else if call.transmitted {
                RpcError::new(ErrorReason::Disconnected, Execution::MaybeExecuted)
            } else {
                RpcError::not_admitted(ErrorReason::Disconnected)
            };
            let _ = call.sender.send(Err(error));
        }
    }

    fn stats(&self) -> RpcStats {
        let state = self.lock();
        self.counters
            .snapshot(state.registry.live(), state.pending.len())
    }
}

impl<P: Providers> EndpointOwner for Shared<P> {
    fn unregister(&self, token: EndpointToken) {
        let removed = self.lock().registry.remove(token);
        // Dropped outside the lock: closing the inbox breaks queued promises.
        drop(removed);
        tracing::debug!(%token, "rpc endpoint destroyed");
    }
}

impl<P: Providers> CallOwner for Shared<P> {
    fn abandon(&self, call_id: u64) -> Option<bool> {
        let call = self.lock().pending.remove(&call_id)?;
        Counters::bump(&self.counters.calls_abandoned);
        tracing::debug!(call_id, "rpc call abandoned by its caller");
        Some(call.transmitted)
    }
}

impl<P: Providers> LocalSink for Shared<P> {
    fn complete_local(&self, call_id: u64, outcome: WireOutcome) {
        self.complete(Origin::Local, call_id, outcome);
    }
}

/// One request presented for admission, from either route.
struct Admission<'a> {
    incarnation: Incarnation,
    token: EndpointToken,
    identity: CallIdentity,
    body: &'a [u8],
}

fn request_frame(
    call_id: u64,
    endpoint: &Endpoint,
    identity: CallIdentity,
    body: Vec<u8>,
    max_frame_bytes: u32,
) -> Result<Vec<u8>, RpcError> {
    let payload = encode_message(&WireMessage::Request {
        call_id,
        incarnation: endpoint.incarnation(),
        token: endpoint.token(),
        method: identity.method,
        schema: identity.schema,
        codec: identity.codec,
        metadata: Vec::new(),
        body,
    });
    encode_frame(&payload, max_frame_bytes).map_err(|_| {
        RpcError::not_admitted(ErrorReason::FrameTooLarge {
            size: payload.len() as u64,
            limit: max_frame_bytes,
        })
    })
}

/// A weak, cloneable handle to a running RPC runtime.
///
/// Registers endpoints and binds clients. It never keeps the runtime alive:
/// once the [`RpcDriver`] is dropped every operation fails with
/// [`ErrorReason::Shutdown`] (or returns `None`).
pub struct RpcHandle<P: Providers> {
    shared: Weak<Shared<P>>,
}

impl<P: Providers> Clone for RpcHandle<P> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<P: Providers> std::fmt::Debug for RpcHandle<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcHandle")
            .field("running", &self.is_running())
            .finish()
    }
}

impl<P: Providers> RpcHandle<P> {
    pub(crate) fn upgrade(&self) -> Option<Arc<Shared<P>>> {
        self.shared.upgrade()
    }

    /// Register a new dynamic endpoint for `M` with the given access class.
    ///
    /// Returns the serialisable reference to hand to callers and the owned
    /// receiver. Dropping the receiver destroys the endpoint.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::NotListening`] on a client-only runtime,
    /// [`ErrorReason::Overloaded`] when the endpoint budget is exhausted and
    /// [`ErrorReason::Shutdown`] once the driver is gone.
    pub fn register<M: RpcMethod>(
        &self,
        access: AccessClass,
    ) -> Result<(ServiceRef<M>, RequestStream<M>), RpcError> {
        self.upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?
            .register::<M>(access)
    }

    /// Bind a reference to this runtime (same as [`ServiceRef::bind`]).
    #[must_use]
    pub fn client<M: RpcMethod>(&self, target: &ServiceRef<M>) -> ServiceClient<P, M> {
        target.bind(self)
    }

    /// Whether the driver still exists.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.shared.strong_count() > 0
    }

    /// This runtime's incarnation, while it runs.
    #[must_use]
    pub fn incarnation(&self) -> Option<Incarnation> {
        self.upgrade().map(|shared| shared.incarnation)
    }

    /// The address advertised in this runtime's endpoints, if listening.
    #[must_use]
    pub fn address(&self) -> Option<SocketAddr> {
        self.upgrade().and_then(|shared| shared.address)
    }

    /// A snapshot of the counters, while the runtime runs.
    #[must_use]
    pub fn stats(&self) -> Option<RpcStats> {
        self.upgrade().map(|shared| shared.stats())
    }

    /// A drop probe that keeps answering after the runtime is gone.
    #[must_use]
    pub fn probe(&self) -> Option<ResourceProbe> {
        self.upgrade().map(|shared| {
            ResourceProbe::new(Arc::clone(&shared.counters), Arc::downgrade(&shared.alive))
        })
    }
}

/// The owned RPC runtime: poll [`run`](Self::run) to make progress.
///
/// Dropping it (or the `run` future) is the shutdown: listener, connections,
/// registrations and pending calls go with it. `U` upgrades every session
/// ([`Plaintext`] by default).
pub struct RpcDriver<P: Providers, U = Plaintext> {
    shared: Arc<Shared<P>>,
    commands: mpsc::UnboundedReceiver<Command<P>>,
    listener: Option<Listener<P>>,
    upgrade: Arc<U>,
}

impl<P: Providers, U> std::fmt::Debug for RpcDriver<P, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcDriver")
            .field("incarnation", &self.shared.incarnation)
            .field("address", &self.shared.address)
            .finish_non_exhaustive()
    }
}

impl<P: Providers> RpcDriver<P> {
    /// Bind `bind_address` and build a plaintext runtime that serves
    /// endpoints there and can call others.
    ///
    /// # Errors
    ///
    /// The listener's bind error, or `InvalidData` when the bound address
    /// is not a resolved `ip:port`.
    pub async fn listen(
        providers: P,
        bind_address: &str,
        config: RpcConfig,
    ) -> io::Result<(Self, RpcHandle<P>)> {
        Self::listen_with(providers, bind_address, config, Plaintext).await
    }

    /// Build a plaintext runtime that only calls others (it cannot
    /// register).
    #[must_use]
    pub fn client_only(providers: P, config: RpcConfig) -> (Self, RpcHandle<P>) {
        Self::client_only_with(providers, config, Plaintext)
    }
}

impl<P: Providers, U: SessionUpgrade<P>> RpcDriver<P, U> {
    fn build(
        providers: P,
        config: RpcConfig,
        address: Option<SocketAddr>,
        upgrade: U,
    ) -> (Self, RpcHandle<P>) {
        // Protocol identifiers come from the provider's random source (seeded
        // in simulation, OS entropy in production) unless the caller supplies
        // one. Never an authentication.
        let incarnation = config
            .incarnation
            .unwrap_or_else(|| Incarnation::from_raw(providers.random().random::<u128>()));
        let (sender, commands) = mpsc::unbounded();
        let shared = Arc::new_cyclic(|this| Shared {
            providers,
            state: Mutex::new(State {
                registry: Registry::new(config.max_endpoints),
                pending: BTreeMap::new(),
                next_call_id: 0,
                outbound: BTreeMap::new(),
                connections: BTreeMap::new(),
                next_connection_id: 0,
            }),
            config,
            incarnation,
            address,
            commands: sender,
            counters: Arc::new(Counters::default()),
            alive: Arc::new(()),
            this: this.clone(),
        });
        let handle = RpcHandle {
            shared: Arc::downgrade(&shared),
        };
        (
            Self {
                shared,
                commands,
                listener: None,
                upgrade: Arc::new(upgrade),
            },
            handle,
        )
    }

    /// Like [`RpcDriver::listen`], upgrading every session with `upgrade`.
    ///
    /// # Errors
    ///
    /// The listener's bind error, or `InvalidData` when the bound address
    /// is not a resolved `ip:port`.
    pub async fn listen_with(
        providers: P,
        bind_address: &str,
        config: RpcConfig,
        upgrade: U,
    ) -> io::Result<(Self, RpcHandle<P>)> {
        let listener = providers.network().bind(bind_address).await?;
        let address = match config.advertised_address {
            Some(advertised) => advertised,
            None => listener
                .local_addr()?
                .parse::<SocketAddr>()
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("listener address is not a resolved ip:port: {error}"),
                    )
                })?,
        };
        let (mut driver, handle) = Self::build(providers, config, Some(address), upgrade);
        driver.listener = Some(listener);
        Ok((driver, handle))
    }

    /// Like [`RpcDriver::client_only`], upgrading every session with
    /// `upgrade`.
    #[must_use]
    pub fn client_only_with(providers: P, config: RpcConfig, upgrade: U) -> (Self, RpcHandle<P>) {
        Self::build(providers, config, None, upgrade)
    }

    /// A fresh handle to this runtime.
    #[must_use]
    pub fn handle(&self) -> RpcHandle<P> {
        RpcHandle {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// Drive the runtime. Never completes; drop it to shut down.
    pub async fn run(self) {
        let Self {
            shared,
            mut commands,
            listener,
            upgrade,
        } = self;
        let mut children: FuturesUnordered<ChildFuture> = FuturesUnordered::new();
        if let Some(listener) = listener {
            children.push(Box::pin(accept_loop(Arc::clone(&shared), listener)));
        }
        futures::future::poll_fn(|cx| {
            // New work first, so a connection queued by a caller is polled in
            // this same pass.
            while let Poll::Ready(Some(command)) = commands.poll_next_unpin(cx) {
                let shared = Arc::clone(&shared);
                let upgrade = Arc::clone(&upgrade);
                let child: ChildFuture = match command {
                    Command::Connect(connection, address) => {
                        Box::pin(drive_outbound(shared, upgrade, connection, address))
                    }
                    Command::Accepted(connection, stream) => {
                        Box::pin(drive_inbound(shared, upgrade, connection, stream))
                    }
                };
                children.push(child);
            }
            // Returns Pending once every child is parked (or after its
            // cooperative budget, having scheduled a re-poll).
            while let Poll::Ready(Some(())) = children.poll_next_unpin(cx) {}
            Poll::<()>::Pending
        })
        .await;
    }
}

async fn accept_loop<P: Providers>(shared: Arc<Shared<P>>, listener: Listener<P>) {
    let _task = TaskGuard::new(&shared.counters);
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => shared.accept(stream, peer),
            Err(error) => {
                // A listener that fails to accept is not retried in a tight
                // loop: the runtime keeps serving its existing connections
                // and can still call out.
                tracing::warn!(%error, "rpc listener failed, no longer accepting");
                return;
            }
        }
    }
}

async fn drive_outbound<P: Providers, U: SessionUpgrade<P>>(
    shared: Arc<Shared<P>>,
    upgrade: Arc<U>,
    connection: Arc<Connection>,
    address: SocketAddr,
) {
    let _task = TaskGuard::new(&shared.counters);
    let peer = address.to_string();
    let connect = async {
        let stream = shared.providers.network().connect(&peer).await?;
        Connector::connect(upgrade.as_ref(), stream, &peer).await
    };
    let reason = match shared
        .time()
        .timeout(shared.config.connect_timeout, connect)
        .await
    {
        Ok(Ok((stream, context))) => {
            connection.set_peer_context(context);
            run_session(&shared, &connection, stream).await
        }
        Ok(Err(error)) => CloseReason::ConnectFailed(error.to_string()),
        Err(_) => CloseReason::ConnectFailed("connect timed out".into()),
    };
    shared.close_connection(&connection, &reason);
}

async fn drive_inbound<P: Providers, U: SessionUpgrade<P>>(
    shared: Arc<Shared<P>>,
    upgrade: Arc<U>,
    connection: Arc<Connection>,
    stream: Stream<P>,
) {
    let _task = TaskGuard::new(&shared.counters);
    let accept = Acceptor::accept(upgrade.as_ref(), stream, connection.peer());
    let reason = match shared
        .time()
        .timeout(shared.config.handshake_timeout, accept)
        .await
    {
        Ok(Ok((stream, context))) => {
            connection.set_peer_context(context);
            run_session(&shared, &connection, stream).await
        }
        Ok(Err(error)) => CloseReason::Protocol(format!("upgrade failed: {error}")),
        Err(_) => CloseReason::Protocol("upgrade timed out".into()),
    };
    shared.close_connection(&connection, &reason);
}

/// Run the framed session until either direction ends or the peer fails to
/// complete its handshake in time.
async fn run_session<P, S>(
    shared: &Arc<Shared<P>>,
    connection: &Arc<Connection>,
    stream: S,
) -> CloseReason
where
    P: Providers,
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    Counters::bump(&shared.counters.connections_opened);
    let (reader, writer) = stream.split();
    let read = read_loop(
        reader,
        shared.config.max_frame_bytes,
        shared.config.read_chunk_bytes,
        |payload| {
            let message = decode_message(&payload)
                .map_err(|error| CloseReason::Protocol(format!("bad envelope: {error}")))?;
            shared.on_message(connection, message)
        },
    );
    let write = write_loop(connection, writer, |call_id| {
        shared.mark_transmitted(call_id);
    });
    let deadline = handshake_deadline(shared, connection, shared.config.handshake_timeout);
    futures::pin_mut!(read, write, deadline);
    let io = futures::future::select(read, write);
    match futures::future::select(io, deadline).await {
        futures::future::Either::Left((
            futures::future::Either::Left((reason, _))
            | futures::future::Either::Right((reason, _)),
            _,
        ))
        | futures::future::Either::Right((reason, _)) => reason,
    }
}

/// Resolves only if the peer's `Hello` has not arrived within `timeout`.
async fn handshake_deadline<P: Providers>(
    shared: &Arc<Shared<P>>,
    connection: &Arc<Connection>,
    timeout: Duration,
) -> CloseReason {
    let _ = shared.time().sleep(timeout).await;
    if !connection.is_established() {
        return CloseReason::Protocol("handshake timed out".into());
    }
    futures::future::pending().await
}
