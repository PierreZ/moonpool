//! A channel that owns its connection and rebuilds it after a loss.

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hyper::body::{Body, Incoming};
use hyper::rt::bounds::Http2ClientConnExec;
use hyper::{Request, Response};
use moonpool_core::{Detach, NetworkProvider, Providers, TaskProvider, TimeProvider};
use tracing::{Instrument, instrument};

use super::channel::{H2Channel, ResponseFuture};
use crate::config::KeepAlive;
use crate::error::ChannelError;
use crate::io::HyperIo;
use crate::rt::{HyperExecutor, HyperTimer};

/// The hyper IO type a channel builds over a provider's stream.
type ChannelIo<P> = HyperIo<<<P as Providers>::Network as NetworkProvider>::TcpStream>;

/// How a [`ReconnectingChannel`] connects and reconnects.
///
/// The defaults mirror moonpool-transport's `PeerConfig`, so a service that
/// speaks both protocols behaves the same way on both.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    /// Initial delay before attempting reconnection.
    ///
    /// Doubles per consecutive failure, up to `max_reconnect_delay`.
    ///
    /// Doubles as the stability threshold: a connection that stays up at least
    /// this long has proved itself and clears the failure count, while one that
    /// dies younger with an error counts as another failure. That is what keeps
    /// a peer which accepts and immediately dies from reconnecting forever at
    /// zero backoff.
    pub initial_reconnect_delay: Duration,

    /// Maximum delay between reconnection attempts.
    pub max_reconnect_delay: Duration,

    /// Timeout for connection attempts.
    ///
    /// Applied to the TCP connect and, separately, to the h2 handshake: a peer
    /// that accepts the connection and then goes quiet must not park the
    /// connection task forever.
    pub connection_timeout: Duration,

    /// Maximum number of consecutive connection failures before giving up.
    /// `None`, the default, retries forever.
    pub max_connection_failures: Option<u32>,

    /// h2 PING keepalive.
    ///
    /// `None`, the default, is hyper's own default: no keepalive, so a peer
    /// that goes silent without closing the socket is only noticed when a
    /// request fails.
    pub keep_alive: Option<KeepAlive>,

    /// Whether the connected stream claims efficient vectored writes.
    ///
    /// Passed to [`HyperIo::with_vectored_writes`]. Default `false`, matching
    /// `HyperIo`'s own default: futures-io cannot be asked whether the
    /// underlying stream really implements `poll_write_vectored`.
    pub vectored_writes: bool,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            connection_timeout: Duration::from_secs(5),
            max_connection_failures: None,
            keep_alive: None,
            vectored_writes: false,
        }
    }
}

/// How long to wait before the attempt that follows `failures` consecutive
/// failures: `initial_reconnect_delay * 2^(failures - 1)`, capped at
/// `max_reconnect_delay`.
///
/// No jitter, by design. Production jitter desynchronizes a fleet of clients;
/// here it would only make reconnect timing depend on something other than the
/// seed. The doubling saturates rather than overflowing, and the loop is
/// bounded by the ceiling rather than by `failures`.
fn backoff_delay(failures: u32, config: &ChannelConfig) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let mut delay = config.initial_reconnect_delay;
    for _ in 1..failures {
        if delay >= config.max_reconnect_delay {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    delay.min(config.max_reconnect_delay)
}

/// A tower [`Service`](tower_service::Service) that keeps an h2 connection to
/// one address, reconnecting as needed.
///
/// This plays the role `tonic::transport::Channel` plays in production: hand it
/// to a generated gRPC client (or any tower stack) and it connects on first
/// use, serves requests over the live connection, and rebuilds the connection
/// after a loss with deterministic backoff. Cloning shares one connection and
/// one reconnection state machine.
///
/// # Readiness and errors
///
/// `poll_ready` drives everything. It returns `Ready(Ok)` only with a live
/// connection, reserving a handle that the following `call` consumes;
/// `Pending` while a connection is being established, waking parked callers in
/// the order they parked; and `Ready(Err)` once per failed attempt, after which
/// the next poll starts a fresh attempt. The channel never retries a *request*
/// on the caller's behalf, which is the correct gRPC semantic: only the caller
/// knows whether its RPC is idempotent.
///
/// # Reconnect accounting
///
/// The consecutive-failure count that drives the backoff and
/// `max_connection_failures` is cleared only once a connection has survived
/// [`initial_reconnect_delay`](ChannelConfig::initial_reconnect_delay) of
/// provider time, not when the handshake completes. A peer that accepts and
/// then dies immediately therefore faces escalating backoff and can exhaust the
/// failure cap, instead of spinning at zero backoff forever.
///
/// This diverges deliberately from moonpool-transport's `Peer`, which resets
/// its backoff at connect time (`peer/core.rs:1582`) and so carries that flap
/// exposure. The two are otherwise configured alike.
///
/// # Where it may be polled
///
/// `poll_ready` spawns the connection task through the task provider, so the
/// channel must be polled from inside a provider task, exactly as calling
/// `tokio::spawn` requires a tokio runtime.
///
/// One task exists per connection: it waits out the backoff, connects, then
/// drives the connection and exits when the connection ends. A channel nobody
/// polls starts nothing, and with the default `keep_alive` of `None` an
/// established connection generates no timer traffic of its own, so a quiesced
/// simulation stays quiesced. Dropping every clone does not close a live
/// connection: the task keeps driving it (keepalive pings included, when
/// configured) until the peer closes it or the process goes away.
pub struct ReconnectingChannel<P: Providers, B> {
    shared: Arc<Shared<P, B>>,

    /// Handle reserved by this clone's last successful `poll_ready`, consumed
    /// by the next `call`. Per clone, not shared: tower's readiness contract is
    /// between one service handle and one request.
    ready: Option<H2Channel<B>>,
}

// Manual: the derive would demand `B: Clone`. A clone shares the connection
// state but starts with no reservation of its own.
impl<P: Providers, B> Clone for ReconnectingChannel<P, B> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            ready: None,
        }
    }
}

impl<P: Providers, B> std::fmt::Debug for ReconnectingChannel<P, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconnectingChannel")
            .field("addr", &self.shared.addr)
            .field("reserved", &self.ready.is_some())
            .finish_non_exhaustive()
    }
}

/// State shared by every clone of a channel and by the connection task.
struct Shared<P: Providers, B> {
    providers: P,
    addr: String,
    config: ChannelConfig,
    inner: Mutex<Inner<B>>,
}

impl<P: Providers, B> Shared<P, B> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<B>> {
        self.inner
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }
}

/// The mutable half. The lock is never held across an await point: the
/// connection task reads what it needs, releases, and takes the lock again only
/// to publish an outcome.
struct Inner<B> {
    conn: Conn<B>,

    /// Consecutive failed attempts, reset by a successful handshake. Drives
    /// both the backoff and `max_connection_failures`. A connection that ends
    /// after being established is not a failure and does not touch this.
    failures: u32,

    /// Incremented every time a connection is published. A connection task
    /// remembers the value it published under and only retires the state if it
    /// still stands, so a driver that notices its socket is gone long after it
    /// was replaced cannot demote a newer connection.
    generation: u64,

    /// Set by a failed attempt, taken by the next `poll_ready`.
    last_error: Option<ChannelError>,

    /// Callers parked in `poll_ready`, woken in the order they parked so wake
    /// ordering is a function of poll order alone.
    wakers: Vec<Waker>,
}

/// Connection state.
enum Conn<B> {
    /// No connection and no attempt running.
    Disconnected,
    /// A connection task is running: waiting out backoff, connecting, or
    /// handshaking.
    Connecting,
    /// A live connection. Requests are served by cloning this handle.
    Connected(H2Channel<B>),
}

impl<P, B> ReconnectingChannel<P, B>
where
    P: Providers,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    HyperExecutor<P::Task>: Http2ClientConnExec<B, ChannelIo<P>>,
{
    /// Create a channel to `addr`. Nothing connects until the channel is first
    /// polled ready.
    #[instrument(skip_all)]
    pub fn new(providers: &P, addr: impl Into<String>, config: ChannelConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                providers: providers.clone(),
                addr: addr.into(),
                config,
                inner: Mutex::new(Inner {
                    conn: Conn::Disconnected,
                    failures: 0,
                    generation: 0,
                    last_error: None,
                    wakers: Vec::new(),
                }),
            }),
            ready: None,
        }
    }

    fn poll_connection(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelError>> {
        // Everything that touches shared state happens in here, and the guard
        // is gone before anything is spawned: the channel lock is never held
        // while the task provider takes its own.
        let start_attempt = {
            let mut inner = self.shared.lock();

            if matches!(&inner.conn, Conn::Connected(c) if c.is_closed()) {
                // A connection that died since the last poll is demoted here
                // rather than waited on, so the reconnect starts in this very
                // poll. It is not a failed attempt: neither the failure count
                // nor the backoff is touched, and no error is surfaced.
                inner.conn = Conn::Disconnected;
                self.ready = None;
            } else if let Conn::Connected(channel) = &inner.conn {
                // Reserve a handle for the call that follows.
                self.ready = Some(channel.clone());
                return Poll::Ready(Ok(()));
            }

            let mut start_attempt = false;
            if matches!(inner.conn, Conn::Disconnected) {
                // A failed attempt is reported to exactly one caller; everyone
                // else, and this caller's next poll, goes on to start a fresh
                // attempt.
                if let Some(error) = inner.last_error.take() {
                    return Poll::Ready(Err(error));
                }

                if let Some(max) = self.shared.config.max_connection_failures
                    && inner.failures >= max
                {
                    return Poll::Ready(Err(ChannelError::ExhaustedRetries {
                        failures: inner.failures,
                    }));
                }

                // Single flight: flipping to Connecting under the lock means
                // the next caller to arrive parks instead of starting a second
                // attempt.
                inner.conn = Conn::Connecting;
                start_attempt = true;
            }

            if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                inner.wakers.push(cx.waker().clone());
            }
            start_attempt
        };

        if start_attempt {
            let shared = Arc::clone(&self.shared);
            self.shared
                .providers
                .task()
                .spawn_task("h2-channel", connect_and_serve(shared))
                .detach();
        }
        Poll::Pending
    }
}

impl<P: Providers, B> ReconnectingChannel<P, B> {
    /// The address this channel connects to.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.shared.addr
    }

    /// Whether a live connection is currently established.
    ///
    /// A hint for tests and logging: the answer can be stale by the time the
    /// caller reads it, and only `poll_ready` establishes a connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(&self.shared.lock().conn, Conn::Connected(c) if !c.is_closed())
    }
}

impl<P, B> tower_service::Service<Request<B>> for ReconnectingChannel<P, B>
where
    P: Providers,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    HyperExecutor<P::Task>: Http2ClientConnExec<B, ChannelIo<P>>,
{
    type Response = Response<Incoming>;
    type Error = ChannelError;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_connection(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        // The reservation from poll_ready is consumed here, and the request
        // then lives or dies with that handle: a reconnection underneath does
        // not rescue it, and its failure does not disturb the channel state.
        match self.ready.take() {
            Some(mut channel) => tower_service::Service::call(&mut channel, req),
            None => ResponseFuture::failed(ChannelError::NotReady),
        }
    }
}

/// Wait out any backoff, connect, publish the connection, then drive it until
/// it ends. One provider task per attempt and its connection.
async fn connect_and_serve<P, B>(shared: Arc<Shared<P, B>>)
where
    P: Providers,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    HyperExecutor<P::Task>: Http2ClientConnExec<B, ChannelIo<P>>,
{
    let addr = shared.addr.clone();
    let (failures, generation) = {
        let inner = shared.lock();
        (inner.failures, inner.generation)
    };
    let attempt = failures.saturating_add(1);

    // Armed before the first await: if this task is dropped mid-attempt
    // (executor shutdown, cancellation), none of the write-backs below run and
    // the channel would sit in Connecting forever with parked callers behind a
    // task that no longer exists.
    let _guard = AttemptGuard {
        shared: Arc::clone(&shared),
        generation,
    };

    // Backoff is served here, never in the caller: a caller that stops polling
    // must not leave the schedule half-applied.
    if failures > 0
        && shared
            .providers
            .time()
            .sleep(backoff_delay(failures, &shared.config))
            .await
            .is_err()
    {
        // The provider is shutting down. Returning here drops the guard, which
        // releases the channel from Connecting and wakes the parked callers
        // without counting this as a failed attempt.
        return;
    }

    tracing::info!(addr = %addr, attempt, "h2_channel_connecting");

    let span = tracing::info_span!("h2_connect", addr = %addr, attempt);
    let outcome = connect(&shared).instrument(span).await;

    let (sender, connection) = match outcome {
        Ok(pair) => pair,
        Err(error) => {
            record_failure(&shared, &addr, attempt, error);
            return;
        }
    };

    // The failure count is deliberately NOT reset here: a handshake proves
    // nothing about a peer that accepts and immediately dies. It is settled
    // below, once the connection's lifetime is known.
    let (my_generation, parked) = {
        let mut inner = shared.lock();
        inner.generation = inner.generation.wrapping_add(1);
        inner.conn = Conn::Connected(H2Channel::new(sender));
        (inner.generation, take_wakers(&mut inner))
    };
    let published_at = shared.providers.time().now();
    wake_all(parked);
    tracing::info!(addr = %addr, "h2_channel_connected");

    // Requests only make progress while this future is polled, so the task
    // stays here for the life of the connection and exits when it ends.
    let ended_with_error = match connection.await {
        Ok(()) => {
            tracing::info!(addr = %addr, "h2_channel_closed");
            false
        }
        Err(error) => {
            tracing::warn!(addr = %addr, detail = %error, "h2_channel_connection_error");
            true
        }
    };
    let alive = shared.providers.time().now().saturating_sub(published_at);

    let parked = {
        let mut inner = shared.lock();
        // Retire only the state this task owns. The generation check rejects a
        // driver whose connection was already replaced; the state check rejects
        // one whose connection was demoted by a poll_ready that has already
        // started a replacement attempt, which would otherwise clobber
        // Connecting and let a second task be spawned.
        if inner.generation != my_generation || !matches!(inner.conn, Conn::Connected(_)) {
            return;
        }
        inner.conn = Conn::Disconnected;
        // Settle the reconnect accounting now that the connection's lifetime is
        // known. No stored error either way: a connection that ends is not a
        // failed connect attempt, so the next poll_ready reconnects rather than
        // reporting anything, though it may now have a backoff to wait out.
        inner.failures = next_failures(inner.failures, alive, ended_with_error, &shared.config);
        take_wakers(&mut inner)
    };
    wake_all(parked);
}

/// The consecutive-failure count after a connection ends, given how long it
/// stayed up and how it ended.
///
/// Resetting the count on a successful handshake would let a peer that accepts
/// and then immediately dies reconnect forever at zero backoff, with
/// `max_connection_failures` never reached. A connection has to earn the reset
/// by surviving `initial_reconnect_delay`; one that dies younger than that with
/// an error counts as another failure and escalates the backoff. A young but
/// clean close leaves the count alone, since the client is usually the one who
/// hung up.
fn next_failures(
    current: u32,
    alive: Duration,
    ended_with_error: bool,
    config: &ChannelConfig,
) -> u32 {
    if alive >= config.initial_reconnect_delay {
        0
    } else if ended_with_error {
        current.saturating_add(1)
    } else {
        current
    }
}

/// Releases the channel from `Connecting` if the connection task dies before
/// publishing.
///
/// Every normal path disarms this by leaving a state other than `Connecting`:
/// a published connection is `Connected` (and bumps the generation), and a
/// failed attempt is `Disconnected`. What is left is the abnormal path, a task
/// dropped mid-attempt, where nothing else would ever move the channel.
struct AttemptGuard<P: Providers, B> {
    shared: Arc<Shared<P, B>>,

    /// The generation current when the attempt began. A mismatch means this
    /// task already published (and something newer may own the state), so the
    /// guard keeps its hands off.
    generation: u64,
}

impl<P: Providers, B> Drop for AttemptGuard<P, B> {
    fn drop(&mut self) {
        let parked = {
            let mut inner = self.shared.lock();
            if inner.generation != self.generation || !matches!(inner.conn, Conn::Connecting) {
                return;
            }
            // Not a failed attempt: the attempt never got a verdict, so the
            // failure count and the backoff stay as they were and the next
            // poll_ready starts a fresh attempt immediately.
            inner.conn = Conn::Disconnected;
            take_wakers(&mut inner)
        };
        wake_all(parked);
    }
}

/// One connection attempt: TCP connect, then the h2 handshake.
async fn connect<P, B>(
    shared: &Shared<P, B>,
) -> Result<
    (
        hyper::client::conn::http2::SendRequest<B>,
        hyper::client::conn::http2::Connection<ChannelIo<P>, B, HyperExecutor<P::Task>>,
    ),
    ChannelError,
>
where
    P: Providers,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    HyperExecutor<P::Task>: Http2ClientConnExec<B, ChannelIo<P>>,
{
    let stream = shared
        .providers
        .time()
        .timeout(
            shared.config.connection_timeout,
            shared.providers.network().connect(&shared.addr),
        )
        .await
        .map_err(|_| ChannelError::ConnectTimeout {
            addr: shared.addr.clone(),
        })?
        .map_err(|e| ChannelError::Connect {
            addr: shared.addr.clone(),
            detail: e.to_string(),
        })?;

    let mut builder = hyper::client::conn::http2::Builder::new(HyperExecutor::new(
        shared.providers.task().clone(),
    ));
    builder.timer(HyperTimer::new(shared.providers.time().clone()));
    if let Some(keep_alive) = &shared.config.keep_alive {
        builder
            .keep_alive_interval(keep_alive.interval)
            .keep_alive_timeout(keep_alive.timeout)
            .keep_alive_while_idle(keep_alive.while_idle);
    }

    let io = HyperIo::new(stream).with_vectored_writes(shared.config.vectored_writes);

    // The handshake gets its own budget: a peer that accepts the connection and
    // then never answers the h2 preface would otherwise park this task forever.
    shared
        .providers
        .time()
        .timeout(shared.config.connection_timeout, builder.handshake(io))
        .await
        .map_err(|_| ChannelError::Handshake {
            addr: shared.addr.clone(),
            detail: "handshake timed out".to_owned(),
        })?
        .map_err(|e| ChannelError::Handshake {
            addr: shared.addr.clone(),
            detail: e.to_string(),
        })
}

/// Record a failed attempt and hand the error to the waiting callers.
fn record_failure<P: Providers, B>(
    shared: &Shared<P, B>,
    addr: &str,
    attempt: u32,
    error: ChannelError,
) {
    let detail = error.to_string();
    let (delay, parked) = {
        let mut inner = shared.lock();
        inner.failures = inner.failures.saturating_add(1);
        inner.conn = Conn::Disconnected;
        inner.last_error = Some(error);
        let delay = backoff_delay(inner.failures, &shared.config);
        (delay, take_wakers(&mut inner))
    };

    tracing::info!(
        addr = %addr,
        attempt,
        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        detail = %detail,
        "h2_channel_connect_failed"
    );
    wake_all(parked);
}

/// Collect the parked callers, oldest first.
fn take_wakers<B>(inner: &mut Inner<B>) -> Vec<Waker> {
    std::mem::take(&mut inner.wakers)
}

/// Wake parked callers in the order they parked.
///
/// Called with the channel lock released: an executor that polls inline from
/// `wake` would otherwise re-enter `poll_ready` and deadlock on the mutex.
fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Waker;
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::Full;
    use moonpool_core::TokioProviders;
    use tower_service::Service as _;

    use super::{
        AttemptGuard, ChannelConfig, Conn, Inner, ReconnectingChannel, Shared, backoff_delay,
        next_failures,
    };
    use crate::ChannelError;

    /// The channel a production tokio stack would build. Naming this type at
    /// all is the point: it makes the compiler discharge hyper's sealed
    /// `Http2ClientConnExec` bound for `HyperExecutor<TokioTaskProvider>` over a
    /// real provider stream, which no generic definition can prove on its own.
    type TestChannel = ReconnectingChannel<TokioProviders, Full<Bytes>>;

    fn test_channel(addr: &str) -> TestChannel {
        ReconnectingChannel::new(&TokioProviders::new(), addr, ChannelConfig::default())
    }

    #[test]
    fn defaults_mirror_the_peer_config() {
        let config = ChannelConfig::default();
        assert_eq!(config.initial_reconnect_delay, Duration::from_millis(100));
        assert_eq!(config.max_reconnect_delay, Duration::from_secs(30));
        assert_eq!(config.connection_timeout, Duration::from_secs(5));
        assert!(config.max_connection_failures.is_none());
        assert!(config.keep_alive.is_none());
        assert!(!config.vectored_writes);
    }

    #[test]
    fn the_first_attempt_does_not_wait() {
        assert_eq!(backoff_delay(0, &ChannelConfig::default()), Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_per_failure() {
        let config = ChannelConfig::default();
        assert_eq!(backoff_delay(1, &config), Duration::from_millis(100));
        assert_eq!(backoff_delay(2, &config), Duration::from_millis(200));
        assert_eq!(backoff_delay(3, &config), Duration::from_millis(400));
        assert_eq!(backoff_delay(4, &config), Duration::from_millis(800));
    }

    #[test]
    fn backoff_saturates_at_the_ceiling() {
        let config = ChannelConfig::default();
        // 100ms doubled eight times is 25.6s; once more would overshoot 30s.
        assert_eq!(backoff_delay(9, &config), Duration::from_millis(25_600));
        assert_eq!(backoff_delay(10, &config), Duration::from_secs(30));
        assert_eq!(backoff_delay(1_000, &config), Duration::from_secs(30));
        // No overflow panic and no unbounded loop at the extreme.
        assert_eq!(backoff_delay(u32::MAX, &config), Duration::from_secs(30));
    }

    #[test]
    fn an_initial_delay_above_the_ceiling_is_clamped() {
        let config = ChannelConfig {
            initial_reconnect_delay: Duration::from_secs(45),
            max_reconnect_delay: Duration::from_secs(30),
            ..ChannelConfig::default()
        };
        assert_eq!(backoff_delay(1, &config), Duration::from_secs(30));
        assert_eq!(backoff_delay(5, &config), Duration::from_secs(30));
    }

    #[test]
    fn call_without_a_reservation_fails_instead_of_panicking() {
        let mut channel = test_channel("10.0.0.1:50051");
        assert_eq!(channel.addr(), "10.0.0.1:50051");
        assert!(!channel.is_connected());

        // No poll_ready first, so no reservation and no task provider involved:
        // the request must resolve to NotReady rather than panic or hang.
        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = futures::executor::block_on(channel.call(request));
        assert!(matches!(response, Err(ChannelError::NotReady)));
    }

    #[test]
    fn clones_share_state_but_not_the_reservation() {
        let channel = test_channel("10.0.0.2:50051");
        let clone = channel.clone();
        assert!(Arc::ptr_eq(&channel.shared, &clone.shared));
        assert!(clone.ready.is_none());
    }

    // ===== the stability rule =====

    #[test]
    fn a_connection_that_survives_the_threshold_clears_the_failures() {
        let config = ChannelConfig::default();
        assert_eq!(
            next_failures(4, config.initial_reconnect_delay, true, &config),
            0
        );
        assert_eq!(next_failures(4, Duration::from_secs(45), false, &config), 0);
    }

    #[test]
    fn an_early_error_death_counts_as_another_failure() {
        let config = ChannelConfig::default();
        // Accepted the handshake, died 1ms later: the flap this rule exists for.
        assert_eq!(next_failures(0, Duration::from_millis(1), true, &config), 1);
        assert_eq!(
            next_failures(3, Duration::from_millis(99), true, &config),
            4
        );
    }

    #[test]
    fn an_early_clean_close_leaves_the_failures_alone() {
        let config = ChannelConfig::default();
        // The client is usually the one that hung up; not the peer's fault.
        assert_eq!(
            next_failures(0, Duration::from_millis(1), false, &config),
            0
        );
        assert_eq!(
            next_failures(3, Duration::from_millis(99), false, &config),
            3
        );
    }

    #[test]
    fn the_failure_count_saturates() {
        let config = ChannelConfig::default();
        assert_eq!(
            next_failures(u32::MAX, Duration::from_millis(1), true, &config),
            u32::MAX
        );
    }

    // ===== the cancellation guard =====

    /// A waker that records whether it was woken.
    struct RecordingWaker(AtomicBool);

    impl std::task::Wake for RecordingWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Shared state in a chosen connection state, with one caller parked.
    fn parked_shared(
        conn: Conn<Full<Bytes>>,
        generation: u64,
    ) -> (
        Arc<Shared<TokioProviders, Full<Bytes>>>,
        Arc<RecordingWaker>,
    ) {
        let flag = Arc::new(RecordingWaker(AtomicBool::new(false)));
        let shared = Arc::new(Shared {
            providers: TokioProviders::new(),
            addr: "10.0.0.3:50051".to_owned(),
            config: ChannelConfig::default(),
            inner: Mutex::new(Inner {
                conn,
                failures: 2,
                generation,
                last_error: None,
                wakers: vec![Waker::from(Arc::clone(&flag))],
            }),
        });
        (shared, flag)
    }

    #[test]
    fn a_task_dropped_mid_attempt_releases_the_channel() {
        let (shared, flag) = parked_shared(Conn::Connecting, 7);

        drop(AttemptGuard {
            shared: Arc::clone(&shared),
            generation: 7,
        });

        let inner = shared.lock();
        assert!(matches!(inner.conn, Conn::Disconnected));
        assert!(inner.wakers.is_empty());
        assert!(flag.0.load(Ordering::SeqCst), "parked caller must be woken");
        // The attempt never reached a verdict, so it is not counted as a
        // failure and the backoff schedule is untouched.
        assert_eq!(inner.failures, 2);
    }

    #[test]
    fn the_guard_leaves_a_newer_generation_alone() {
        let (shared, flag) = parked_shared(Conn::Connecting, 9);

        // This task published (generation moved on) before being dropped.
        drop(AttemptGuard {
            shared: Arc::clone(&shared),
            generation: 7,
        });

        let inner = shared.lock();
        assert!(matches!(inner.conn, Conn::Connecting));
        assert_eq!(inner.wakers.len(), 1);
        assert!(!flag.0.load(Ordering::SeqCst));
    }

    #[test]
    fn the_guard_only_acts_on_an_attempt_in_flight() {
        // Disconnected stands in for every non-Connecting state: the guard
        // checks for Connecting specifically. Conn::Connected cannot be built
        // in a unit test, since a SendRequest only comes from a real handshake.
        let (shared, flag) = parked_shared(Conn::Disconnected, 7);

        drop(AttemptGuard {
            shared: Arc::clone(&shared),
            generation: 7,
        });

        let inner = shared.lock();
        assert_eq!(inner.wakers.len(), 1);
        assert!(!flag.0.load(Ordering::SeqCst));
    }
}
