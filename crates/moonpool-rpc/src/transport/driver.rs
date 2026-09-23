//! The owned driver: the accept loop, dialing with backoff, and each
//! session's read, write, handshake and liveness futures.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures::channel::mpsc;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::FuturesUnordered;
use futures::{AsyncReadExt, StreamExt};
use moonpool_core::{NetworkProvider, Providers, RandomProvider, TcpListenerTrait, TimeProvider};

use super::connection::{CloseReason, Connection, read_loop, write_loop};
use super::handle::RpcHandle;
use super::peer::jittered;
use super::upgrade::{Acceptor, Connector, Plaintext};
use super::{Command, Listener, SessionUpgrade, Shared, Stream};
use crate::config::RpcConfig;
use crate::protocol::decode_message;
use crate::stats::{Counters, TaskGuard};

type ChildFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

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
    /// `InvalidInput` when the configuration does not validate
    /// ([`RpcConfig::validate`]), the listener's bind error, or
    /// `InvalidData` when the bound address is not a resolved `ip:port`.
    pub async fn listen(
        providers: P,
        bind_address: &str,
        config: RpcConfig,
    ) -> io::Result<(Self, RpcHandle<P>)> {
        Self::listen_with(providers, bind_address, config, Plaintext).await
    }

    /// Build a plaintext runtime that only calls others (it cannot
    /// register).
    ///
    /// # Errors
    ///
    /// `InvalidInput` when the configuration does not validate
    /// ([`RpcConfig::validate`]).
    pub fn client_only(providers: P, config: RpcConfig) -> io::Result<(Self, RpcHandle<P>)> {
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
        let (sender, commands) = mpsc::unbounded();
        let shared = Shared::new(providers, config, address, sender);
        let handle = RpcHandle::new(&shared);
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
    /// `InvalidInput` when the configuration does not validate, the
    /// listener's bind error, or `InvalidData` when the bound address is not
    /// a resolved `ip:port`.
    pub async fn listen_with(
        providers: P,
        bind_address: &str,
        config: RpcConfig,
        upgrade: U,
    ) -> io::Result<(Self, RpcHandle<P>)> {
        validated(&config)?;
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
    ///
    /// # Errors
    ///
    /// `InvalidInput` when the configuration does not validate.
    pub fn client_only_with(
        providers: P,
        config: RpcConfig,
        upgrade: U,
    ) -> io::Result<(Self, RpcHandle<P>)> {
        validated(&config)?;
        Ok(Self::build(providers, config, None, upgrade))
    }

    /// A fresh handle to this runtime.
    #[must_use]
    pub fn handle(&self) -> RpcHandle<P> {
        RpcHandle::new(&self.shared)
    }

    /// Drive the runtime; drop it to shut down.
    ///
    /// Completes only when the listener fails fatally (see
    /// [`is_transient_accept_error`]), returning that error; transient accept
    /// errors are retried with bounded backoff on provider time. A
    /// client-only runtime never completes.
    pub async fn run(self) -> io::Error {
        let Self {
            shared,
            mut commands,
            listener,
            upgrade,
        } = self;
        let mut children: FuturesUnordered<ChildFuture> = FuturesUnordered::new();
        let mut acceptor: Option<Pin<Box<dyn Future<Output = io::Error> + Send>>> =
            listener.map(|listener| {
                Box::pin(accept_loop(Arc::clone(&shared), listener))
                    as Pin<Box<dyn Future<Output = io::Error> + Send>>
            });
        futures::future::poll_fn(|cx| {
            if let Some(accepting) = acceptor.as_mut()
                && let Poll::Ready(error) = accepting.as_mut().poll(cx)
            {
                acceptor = None;
                return Poll::Ready(error);
            }
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
            Poll::Pending
        })
        .await
    }
}

/// Whether an accept error is worth retrying.
///
/// Per-connection failures (an aborted or reset handshake), interruptions
/// and resource exhaustion (`EMFILE`/`ENFILE`/`ENOBUFS`, which std reports as
/// uncategorised or `OutOfMemory`) pass: the listener itself is fine, and
/// retrying after a pause is what servers do. A listener that is invalid,
/// unsupported, denied or otherwise broken in a way that will not heal is
/// fatal.
#[must_use]
pub fn is_transient_accept_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::Unsupported
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotConnected
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::AddrInUse
            | io::ErrorKind::NotFound
    )
}

/// First pause after a transient accept error; doubles up to the maximum.
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(5);
/// Longest pause between accept retries.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

fn validated(config: &RpcConfig) -> io::Result<()> {
    config
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

async fn accept_loop<P: Providers>(shared: Arc<Shared<P>>, listener: Listener<P>) -> io::Error {
    let _task = TaskGuard::new(&shared.counters);
    let mut backoff = ACCEPT_BACKOFF_MIN;
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                backoff = ACCEPT_BACKOFF_MIN;
                shared.accept(stream, peer);
            }
            Err(error) if is_transient_accept_error(&error) => {
                Counters::bump(&shared.counters.accept_errors);
                tracing::warn!(%error, ?backoff, "rpc accept failed, retrying");
                // Existing sessions keep running while the listener waits.
                let _ = shared.time().sleep(backoff).await;
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
            }
            Err(error) => {
                Counters::bump(&shared.counters.accept_errors);
                tracing::error!(%error, "rpc listener failed fatally");
                return error;
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
    // Pace dials to this peer (the reconnect backoff). Calls queue on the
    // connection meanwhile and can still be withdrawn untransmitted.
    let wait = shared.dial_delay(address);
    if !wait.is_zero() {
        Counters::bump(&shared.counters.reconnect_waits);
        let _ = shared.time().sleep(wait).await;
    }
    if connection.is_closed() {
        // Replaced (an accepted session was adopted) while waiting.
        shared.close_connection(&connection, CloseReason::Replaced);
        return;
    }
    Counters::bump(&shared.counters.dials);
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
    shared.close_connection(&connection, reason);
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
    shared.close_connection(&connection, reason);
}

impl<P: Providers> Shared<P> {
    /// The jittered pause before dialing `address` now, recorded as that
    /// peer's latest dial.
    fn dial_delay(&self, address: SocketAddr) -> Duration {
        let draw = self.random().random_ratio();
        let now = self.now();
        let mut state = self.lock();
        state
            .peers
            .get_mut(&address)
            .map_or(Duration::ZERO, |peer| {
                peer.next_dial(now, &self.config.peer, draw)
            })
    }

    fn jitter(&self, duration: Duration) -> Duration {
        jittered(
            duration,
            self.config.peer.jitter_percent,
            self.random().random_ratio(),
        )
    }
}

/// Run the framed session until either direction ends, the peer fails to
/// complete its handshake in time, or the liveness monitor gives up.
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
    let batch = shared.config.limits.max_frames_per_batch;
    let read = read_loop(
        reader,
        shared.config.max_frame_bytes,
        shared.config.read_chunk_bytes,
        batch,
        |payload| {
            let message = decode_message(&payload)
                .map_err(|error| CloseReason::Protocol(format!("bad envelope: {error}")))?;
            shared.on_message(connection, message)
        },
    );
    let write = write_loop(connection, writer, batch, |call_id, frame_len| {
        shared.admit_transmit(connection, call_id, frame_len)
    });
    let deadline = handshake_deadline(shared, connection, shared.config.handshake_timeout);
    let liveness = monitor(shared, connection);
    futures::pin_mut!(read, write, deadline, liveness);
    let io = futures::future::select(read, write);
    let guards = futures::future::select(deadline, liveness);
    match futures::future::select(io, guards).await {
        futures::future::Either::Left((
            futures::future::Either::Left((reason, _))
            | futures::future::Either::Right((reason, _)),
            _,
        ))
        | futures::future::Either::Right((
            futures::future::Either::Left((reason, _))
            | futures::future::Either::Right((reason, _)),
            _,
        )) => reason,
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

/// The session's liveness and idleness (`FoundationDB`'s
/// `connectionMonitor`): every jittered ping interval, close the session
/// if it sat idle long enough; on a selected connection, send a ping and
/// fail the session if nothing at all arrives within the ping timeout.
async fn monitor<P: Providers>(
    shared: &Arc<Shared<P>>,
    connection: &Arc<Connection>,
) -> CloseReason {
    let policy = &shared.config.peer;
    loop {
        let _ = shared
            .time()
            .sleep(shared.jitter(policy.ping_interval))
            .await;
        if connection.is_closed() {
            return CloseReason::Local;
        }
        if !connection.is_established() {
            continue;
        }
        if shared.close_if_idle(connection) {
            return CloseReason::Idle;
        }
        if connection.peer_address().is_none()
            && connection.silent_for(shared.now()) < policy.inbound_idle_timeout
        {
            // Served only: the dialer pings us, and idleness reaps it once
            // nothing is owed. But a dialer silent for the inbound idle
            // timeout while replies or streams are still owed to it has
            // vanished behind a half-open session: probe it ourselves and
            // fail the session if nothing answers (`FoundationDB`'s
            // `connectionMonitor` for incoming connections), so the work
            // owed to it is released instead of waiting forever.
            continue;
        }
        let before = connection.received();
        shared.ping(connection);
        let _ = shared.time().sleep(policy.ping_timeout).await;
        if connection.is_closed() {
            return CloseReason::Local;
        }
        if connection.received() == before {
            tracing::debug!(peer = %connection.peer(), "rpc ping timed out");
            return CloseReason::PingTimeout;
        }
    }
}
