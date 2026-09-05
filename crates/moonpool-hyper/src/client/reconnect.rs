//! A channel that owns its connection and rebuilds it after a loss.

use std::error::Error;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures::task::AtomicWaker;
use hyper::body::{Body, Incoming};
use hyper::rt::bounds::Http2ClientConnExec;
use hyper::{Request, Response};
use moonpool_core::{Detach, NetworkProvider, Providers, TaskProvider, TimeProvider};
use tracing::{Instrument, instrument};

use super::channel::{H2Channel, ResponseFuture};
use crate::error::ChannelError;
use crate::io::HyperIo;
use crate::rt::{HyperExecutor, HyperTimer};

use super::config::{ChannelConfig, backoff_delay};
use super::state::{Conn, Inner};

/// The hyper IO type a channel builds over a provider's stream.
type ChannelIo<P> = HyperIo<<<P as Providers>::Network as NetworkProvider>::TcpStream>;

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
/// `poll_ready` drives everything. With a live connection it reserves a handle
/// that the following `call` consumes. While a connection is being established
/// it returns `Pending`, waking parked callers in the order they parked. A
/// failed attempt is also reserved for one caller: readiness succeeds and the
/// following `call` returns that error. This follows tower's rule that a
/// readiness error is terminal for that service instance while still allowing
/// the shared channel to reconnect. `Ready(Err)` is reserved for explicit
/// shutdown and an exhausted connection-failure limit. The channel never
/// retries a *request* on the caller's behalf, which is the correct gRPC
/// semantic: only the caller knows whether its RPC is idempotent.
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
/// Resetting only after a stable interval prevents a peer that accepts and
/// immediately closes from keeping the channel at zero backoff.
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
/// simulation stays quiesced. Call [`close`](Self::close) to stop the shared
/// lifecycle explicitly. Closing is idempotent, affects every clone, and
/// cancels backoff, connection attempts, h2 keepalive work, and the live
/// connection driver.
pub struct ReconnectingChannel<P: Providers, B> {
    shared: Arc<Shared<P, B>>,

    /// Outcome reserved by this clone's last successful `poll_ready`, consumed
    /// by the next `call`. Per clone, not shared: tower's readiness contract is
    /// between one service handle and one request.
    ready: Option<Reservation<B>>,
}

/// The outcome reserved by one clone's successful readiness poll.
enum Reservation<B> {
    /// A live h2 sender for the next request.
    Channel(H2Channel<B>),
    /// One failed connection attempt, surfaced by the next call.
    Failure(ChannelError),
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
            .field("closed", &self.shared.is_closed())
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
    closed: AtomicBool,
    close_waiters: Mutex<Vec<Weak<CloseWaiter>>>,
}

/// One lifecycle task's registration for explicit shutdown.
struct CloseWaiter {
    waker: AtomicWaker,
}

impl<P: Providers, B> Shared<P, B> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<B>> {
        self.inner
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        let parked = {
            let mut inner = self.lock();
            inner.conn = Conn::Closed;
            inner.take_wakers()
        };
        let lifecycle_waiters = {
            let mut waiters = self
                .close_waiters
                .lock()
                .expect("Mutex poisoned: prior task panicked");
            std::mem::take(&mut *waiters)
        };
        for waiter in lifecycle_waiters {
            if let Some(waiter) = waiter.upgrade() {
                waiter.waker.wake();
            }
        }
        wake_all(parked);
    }

    fn poll_closed(&self, waiter: &Arc<CloseWaiter>, cx: &mut Context<'_>) -> Poll<()> {
        if self.is_closed() {
            return Poll::Ready(());
        }

        waiter.waker.register(cx.waker());
        let mut waiters = self
            .close_waiters
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        if self.is_closed() {
            return Poll::Ready(());
        }
        waiters.retain(|registered| registered.strong_count() > 0);
        let registration = Arc::downgrade(waiter);
        if !waiters
            .iter()
            .any(|registered| registered.ptr_eq(&registration))
        {
            waiters.push(registration);
        }
        Poll::Pending
    }
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
                inner: Mutex::new(Inner::new()),
                closed: AtomicBool::new(false),
                close_waiters: Mutex::new(Vec::new()),
            }),
            ready: None,
        }
    }

    fn poll_connection(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelError>> {
        if self.shared.is_closed() {
            self.ready = None;
            return Poll::Ready(Err(ChannelError::Closed));
        }

        // Apart from the explicit terminal close handled above, readiness once
        // granted is never taken back. Tower forbids returning Pending after a
        // Ready(Ok), and middleware that polls again before calling (a balancer
        // picking among ready services, say) would see exactly that if a
        // connection died in between. So a clone holding a reservation stays
        // ready, and touches no shared state: it starts no attempt and consumes
        // no stored error. Calling through a reservation whose connection died
        // fails fast in the request future instead, which the contract allows.
        if self.ready.is_some() {
            if matches!(self.ready, Some(Reservation::Failure(_))) {
                return Poll::Ready(Ok(()));
            }

            let inner = self.shared.lock();
            if self.shared.is_closed() || matches!(inner.conn, Conn::Closed) {
                self.ready = None;
                return Poll::Ready(Err(ChannelError::Closed));
            }
            if let Conn::Connected(channel) = &inner.conn
                && !channel.is_closed()
            {
                // A live connection exists, so hand the caller that one rather
                // than a reservation that may already be stale.
                self.ready = Some(Reservation::Channel(channel.clone()));
            }
            return Poll::Ready(Ok(()));
        }

        // Everything that touches shared state happens in here, and the guard
        // is gone before anything is spawned: the channel lock is never held
        // while the task provider takes its own.
        let start_attempt = {
            let mut inner = self.shared.lock();

            if self.shared.is_closed() || matches!(inner.conn, Conn::Closed) {
                self.ready = None;
                return Poll::Ready(Err(ChannelError::Closed));
            }

            if matches!(&inner.conn, Conn::Connected(c) if c.is_closed()) {
                // A connection that died since the last poll is demoted here
                // rather than waited on, so the reconnect starts in this very
                // poll. It is not a failed attempt: neither the failure count
                // nor the backoff is touched, and no error is surfaced.
                // (No reservation to clear: the early return above owns that
                // case.)
                inner.conn = Conn::Disconnected;
            } else if let Conn::Connected(channel) = &inner.conn {
                // Reserve a handle for the call that follows.
                self.ready = Some(Reservation::Channel(channel.clone()));
                return Poll::Ready(Ok(()));
            }

            if let Some(error) = inner.take_failure() {
                self.ready = Some(Reservation::Failure(error));
                return Poll::Ready(Ok(()));
            }

            let mut start_attempt = false;
            if matches!(inner.conn, Conn::Disconnected) {
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

            inner.park(cx.waker());
            start_attempt
        };

        if start_attempt {
            if self.shared.is_closed() {
                return Poll::Ready(Err(ChannelError::Closed));
            }
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

    /// Shut down this channel and every clone of it.
    ///
    /// The first call wakes pending readiness callers and cancels any backoff,
    /// connection attempt, handshake, live h2 connection, and keepalive work.
    /// In-flight request futures, pending and future readiness checks, and
    /// future calls fail with [`ChannelError::Closed`]. Repeated calls are
    /// harmless.
    pub fn close(&self) {
        self.shared.close();
    }

    /// Whether this channel has been explicitly shut down.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
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
        if self.shared.is_closed() {
            self.ready = None;
            return ResponseFuture::failed(ChannelError::Closed);
        }

        // The reservation from poll_ready is consumed here. A reconnection
        // underneath does not rescue the request, and its failure does not
        // disturb channel state; explicit shutdown is the one shared event the
        // response future also observes.
        match self.ready.take() {
            Some(Reservation::Channel(mut channel)) => {
                let response = tower_service::Service::call(&mut channel, req);
                let shared = Arc::clone(&self.shared);
                ResponseFuture::new(async move {
                    match run_until_closed(shared, response).await {
                        Some(result) => result,
                        None => Err(ChannelError::Closed),
                    }
                })
            }
            Some(Reservation::Failure(error)) => {
                let shared = Arc::clone(&self.shared);
                ResponseFuture::new(async move {
                    let failure = std::future::ready(Err(error));
                    match run_until_closed(shared, failure).await {
                        Some(result) => result,
                        None => Err(ChannelError::Closed),
                    }
                })
            }
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
    let lifecycle = connection_lifecycle(Arc::clone(&shared));
    let _ = run_until_closed(shared, lifecycle).await;
}

/// Run one lifecycle until it completes or explicit channel shutdown wins.
async fn run_until_closed<P, B, F>(shared: Arc<Shared<P, B>>, future: F) -> Option<F::Output>
where
    P: Providers,
    F: Future,
{
    let waiter = Arc::new(CloseWaiter {
        waker: AtomicWaker::new(),
    });
    let closed = futures::future::poll_fn(|cx| shared.poll_closed(&waiter, cx));
    futures::pin_mut!(closed, future);
    match futures::future::select(closed, future).await {
        futures::future::Either::Left((_closed, _future)) => None,
        futures::future::Either::Right((output, _closed)) => Some(output),
    }
}

/// The backoff, connect, handshake, and live-driver work for one attempt.
async fn connection_lifecycle<P, B>(shared: Arc<Shared<P, B>>)
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
        if shared.is_closed() || !matches!(inner.conn, Conn::Connecting) {
            return;
        }
        inner.generation = inner.generation.wrapping_add(1);
        inner.conn = Conn::Connected(H2Channel::new(sender));
        (inner.generation, inner.take_wakers())
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
        inner.take_wakers()
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
            inner.take_wakers()
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
        if shared.is_closed() || matches!(inner.conn, Conn::Closed) {
            return;
        }
        inner.failures = inner.failures.saturating_add(1);
        inner.conn = Conn::Failed(error);
        let delay = backoff_delay(inner.failures, &shared.config);
        (delay, inner.take_wakers())
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
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::Full;
    use moonpool_core::TokioProviders;
    use moonpool_sim::{SimProviders, SimWorld};
    use tower_service::Service as _;

    use super::{
        AttemptGuard, ChannelConfig, Conn, H2Channel, Inner, Providers as _, ReconnectingChannel,
        Reservation, Shared, backoff_delay, next_failures, run_until_closed,
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
    fn defaults_are_conservative() {
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
    fn a_zero_initial_delay_stays_zero_at_the_failure_limit() {
        let config = ChannelConfig {
            initial_reconnect_delay: Duration::ZERO,
            ..ChannelConfig::default()
        };

        // A zero delay cannot grow by doubling. Handle it directly rather than
        // iterating over every failure when the saturating counter reaches its
        // limit.
        assert_eq!(backoff_delay(u32::MAX, &config), Duration::ZERO);
    }

    // ===== readiness reservations =====

    /// Just enough of an h2 server for a client handshake to finish: it answers
    /// the connection preface with an empty SETTINGS frame and then goes quiet,
    /// swallowing everything written to it.
    ///
    /// This exists because `H2Channel` wraps a `SendRequest`, which hyper only
    /// hands out from a real handshake. With one, the reservation arms of
    /// `poll_ready` can be tested against genuine channels rather than stand-ins.
    struct HandshakePeer {
        settings_sent: bool,
    }

    /// A SETTINGS frame with an empty payload: length 0, type 0x04, no flags,
    /// stream 0.
    const EMPTY_SETTINGS: [u8; 9] = [0, 0, 0, 0x04, 0, 0, 0, 0, 0];

    impl futures::io::AsyncRead for HandshakePeer {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.settings_sent {
                // Not EOF: an EOF would close the connection and with it the
                // sender we are trying to obtain.
                return Poll::Pending;
            }
            self.settings_sent = true;
            let n = EMPTY_SETTINGS.len().min(buf.len());
            buf[..n].copy_from_slice(&EMPTY_SETTINGS[..n]);
            Poll::Ready(Ok(n))
        }
    }

    impl futures::io::AsyncWrite for HandshakePeer {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The driver half of a handshake. Holding it keeps the sender open;
    /// dropping it closes the connection, which is how a dead channel is made.
    type TestConnection = hyper::client::conn::http2::Connection<
        crate::io::HyperIo<HandshakePeer>,
        Full<Bytes>,
        crate::rt::HyperExecutor<moonpool_core::TokioTaskProvider>,
    >;

    /// Hand-shake against [`HandshakePeer`] and return both halves.
    ///
    /// Needs a tokio runtime because hyper's h2 handshake spawns an internal
    /// task, which is also why the tests using it are `#[tokio::test]`. The
    /// timeout keeps a handshake that never completes from hanging the suite.
    async fn handshake() -> (H2Channel<Full<Bytes>>, TestConnection) {
        let providers = TokioProviders::new();
        let executor = crate::rt::HyperExecutor::new(providers.task().clone());
        let io = crate::io::HyperIo::new(HandshakePeer {
            settings_sent: false,
        });

        let (sender, connection) = tokio::time::timeout(
            Duration::from_secs(5),
            hyper::client::conn::http2::Builder::new(executor).handshake(io),
        )
        .await
        .expect("handshake against the fake peer timed out")
        .expect("handshake against the fake peer failed");

        (H2Channel::new(sender), connection)
    }

    /// A channel whose shared state and reservation are set up by hand.
    fn channel_with(
        conn: Conn<Full<Bytes>>,
        reservation: Option<H2Channel<Full<Bytes>>>,
    ) -> TestChannel {
        let mut inner = Inner::new();
        inner.conn = conn;
        inner.generation = 1;
        ReconnectingChannel {
            shared: Arc::new(Shared {
                providers: TokioProviders::new(),
                addr: "10.0.0.9:50051".to_owned(),
                config: ChannelConfig::default(),
                inner: Mutex::new(inner),
                closed: AtomicBool::new(false),
                close_waiters: Mutex::new(Vec::new()),
            }),
            ready: reservation.map(Reservation::Channel),
        }
    }

    #[tokio::test]
    async fn a_granted_readiness_is_never_revoked() {
        let (live, connection) = handshake().await;
        assert!(!live.is_closed());

        let mut channel = channel_with(Conn::Connected(live), None);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        // First poll grants readiness and takes a reservation.
        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        assert!(channel.ready.is_some());

        // The connection dies before the caller gets around to calling.
        drop(connection);
        channel.shared.lock().conn = Conn::Disconnected;

        // Tower forbids taking readiness back, so this must still be Ready.
        assert!(
            matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))),
            "poll_ready revoked a readiness it had already granted"
        );
    }

    #[tokio::test]
    async fn a_stale_reservation_is_upgraded_to_the_live_connection() {
        let (stale, stale_connection) = handshake().await;
        drop(stale_connection);
        assert!(
            stale.is_closed(),
            "dropping the driver must close the sender"
        );

        let (live, _live_connection) = handshake().await;
        let mut channel = channel_with(Conn::Connected(live), Some(stale));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        let reserved = channel.ready.as_ref().expect("reservation must survive");
        assert!(
            matches!(reserved, Reservation::Channel(channel) if !channel.is_closed()),
            "a live connection must replace a dead reservation"
        );
    }

    #[tokio::test]
    async fn a_reserved_clone_touches_no_shared_state() {
        let (reservation, connection) = handshake().await;
        drop(connection);

        // Disconnected with an error waiting: if the reserved clone consumed
        // the error or started an attempt, the assertions below would see it.
        let mut channel = channel_with(Conn::Failed(ChannelError::NotReady), Some(reservation));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let inner = channel.shared.lock();
        assert!(
            matches!(inner.conn, Conn::Failed(ChannelError::NotReady)),
            "stored error must not be taken"
        );
        assert!(
            !matches!(inner.conn, Conn::Connecting),
            "no attempt started"
        );
        assert_eq!(inner.parked_count(), 0, "a ready caller must not park");
    }

    #[tokio::test]
    async fn a_failed_attempt_is_reserved_until_call() {
        let mut channel = channel_with(Conn::Failed(ChannelError::NotReady), None);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        assert!(matches!(channel.shared.lock().conn, Conn::Disconnected));
        assert!(matches!(
            channel.ready.as_ref(),
            Some(Reservation::Failure(ChannelError::NotReady))
        ));

        // Re-polling readiness cannot consume the reserved outcome or start a
        // new attempt before tower submits the corresponding request.
        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        assert!(matches!(channel.shared.lock().conn, Conn::Disconnected));

        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = channel.call(request).await;
        assert!(matches!(response, Err(ChannelError::NotReady)));
        assert!(channel.ready.is_none());

        assert!(matches!(channel.poll_ready(&mut cx), Poll::Pending));
        assert!(matches!(channel.shared.lock().conn, Conn::Connecting));
        channel.close();
    }

    #[tokio::test]
    async fn a_reserved_failure_is_not_replaced_by_another_clones_connection() {
        let mut failed = channel_with(Conn::Failed(ChannelError::NotReady), None);
        let mut connected = failed.clone();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(failed.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let (live, _connection) = handshake().await;
        failed.shared.lock().conn = Conn::Connected(live);

        assert!(matches!(failed.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        assert!(matches!(
            failed.ready.as_ref(),
            Some(Reservation::Failure(ChannelError::NotReady))
        ));
        assert!(matches!(connected.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        assert!(matches!(
            connected.ready.as_ref(),
            Some(Reservation::Channel(_))
        ));

        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = failed.call(request).await;
        assert!(matches!(response, Err(ChannelError::NotReady)));
    }

    #[test]
    fn explicit_close_overrides_a_reserved_attempt_failure() {
        let mut channel = channel_with(Conn::Failed(ChannelError::NotReady), None);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = channel.call(request);
        channel.close();

        let result = futures::executor::block_on(response);
        assert!(matches!(result, Err(ChannelError::Closed)));
    }

    #[test]
    fn exhausting_retries_is_a_terminal_readiness_error() {
        let mut channel = channel_with(Conn::Failed(ChannelError::NotReady), None);
        Arc::get_mut(&mut channel.shared)
            .expect("test channel must have one owner")
            .config
            .max_connection_failures = Some(2);
        channel.shared.lock().failures = 2;
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = futures::executor::block_on(channel.call(request));
        assert!(matches!(response, Err(ChannelError::NotReady)));

        assert!(matches!(
            channel.poll_ready(&mut cx),
            Poll::Ready(Err(ChannelError::ExhaustedRetries { failures: 2 }))
        ));
        assert!(channel.ready.is_none());
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

    #[test]
    fn close_is_idempotent_and_shared_by_every_clone() {
        let channel = test_channel("10.0.0.4:50051");
        let mut clone = channel.clone();
        let flag = Arc::new(RecordingWaker(AtomicBool::new(false)));
        channel.shared.lock().park(&Waker::from(Arc::clone(&flag)));

        channel.close();
        clone.close();

        assert!(channel.is_closed());
        assert!(clone.is_closed());
        assert!(matches!(channel.shared.lock().conn, Conn::Closed));
        assert!(flag.0.load(Ordering::SeqCst), "parked caller must be woken");

        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            clone.poll_ready(&mut cx),
            Poll::Ready(Err(ChannelError::Closed))
        ));

        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = futures::executor::block_on(clone.call(request));
        assert!(matches!(response, Err(ChannelError::Closed)));
    }

    #[tokio::test]
    async fn close_cancels_a_live_connection_driver() {
        let (live, connection) = handshake().await;
        let mut channel = channel_with(Conn::Connected(live), None);
        let closer = channel.clone();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let shared = Arc::clone(&channel.shared);
        let driver = tokio::spawn(run_until_closed(Arc::clone(&shared), async move {
            let _ = connection.await;
        }));
        tokio::task::yield_now().await;

        closer.close();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("closed connection driver did not stop")
            .expect("connection driver task panicked");

        assert!(matches!(shared.lock().conn, Conn::Closed));
        assert!(matches!(
            channel.poll_ready(&mut cx),
            Poll::Ready(Err(ChannelError::Closed))
        ));
    }

    #[tokio::test]
    async fn close_fails_an_in_flight_request_deterministically() {
        let (live, _connection) = handshake().await;
        let mut channel = channel_with(Conn::Connected(live), None);
        let closer = channel.clone();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(channel.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = tokio::spawn(channel.call(request));
        tokio::task::yield_now().await;

        closer.close();
        let result = tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .expect("closed request did not wake")
            .expect("request task panicked");
        assert!(matches!(result, Err(ChannelError::Closed)));
    }

    #[tokio::test]
    async fn close_wakes_every_overlapping_lifecycle() {
        let channel = test_channel("10.0.0.5:50051");
        let first = tokio::spawn(run_until_closed(
            Arc::clone(&channel.shared),
            std::future::pending::<()>(),
        ));
        let second = tokio::spawn(run_until_closed(
            Arc::clone(&channel.shared),
            std::future::pending::<()>(),
        ));
        tokio::task::yield_now().await;

        channel.close();
        tokio::time::timeout(Duration::from_secs(5), async {
            first.await.expect("first lifecycle task panicked");
            second.await.expect("second lifecycle task panicked");
        })
        .await
        .expect("not every lifecycle task stopped");
    }

    fn assert_sim_shutdown_cancels_attempt(failures: u32) {
        let sim = SimWorld::new();
        // `SimProviders::new` no longer seeds the thread-local stream; seed it
        // explicitly so the backoff draws stay the ones this test was written on.
        moonpool_sim::set_sim_seed(17);
        let ip = "10.0.1.1".parse().expect("test IP must be valid");
        let providers = SimProviders::new(sim.downgrade(), ip);
        let mut executor = moonpool_sim::executor::Executor::new(17);

        executor.block_on(async {
            let mut channel: ReconnectingChannel<_, Full<Bytes>> =
                ReconnectingChannel::new(&providers, "10.0.1.2:50051", ChannelConfig::default());
            let clone = channel.clone();
            channel.shared.lock().failures = failures;

            futures::future::poll_fn(|cx| {
                assert!(matches!(channel.poll_ready(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;
            moonpool_sim::executor::until_stalled().await;

            assert!(sim.pending_event_count() > 0, "attempt must own sim work");
            assert!(
                Arc::strong_count(&channel.shared) > 2,
                "attempt task must own shared state"
            );

            clone.close();
            moonpool_sim::executor::until_stalled().await;

            assert_eq!(
                sim.pending_event_count(),
                0,
                "shutdown must cancel channel-owned simulation events"
            );
            assert_eq!(
                Arc::strong_count(&channel.shared),
                2,
                "no channel-owned task may survive shutdown"
            );
        });
    }

    #[test]
    fn sim_shutdown_cancels_connect_and_backoff_tasks() {
        assert_sim_shutdown_cancels_attempt(0);
        assert_sim_shutdown_cancels_attempt(1);
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
        let mut inner = Inner::new();
        inner.conn = conn;
        inner.failures = 2;
        inner.generation = generation;
        inner.park(&Waker::from(Arc::clone(&flag)));
        let shared = Arc::new(Shared {
            providers: TokioProviders::new(),
            addr: "10.0.0.3:50051".to_owned(),
            config: ChannelConfig::default(),
            inner: Mutex::new(inner),
            closed: AtomicBool::new(false),
            close_waiters: Mutex::new(Vec::new()),
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
        assert_eq!(inner.parked_count(), 0);
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
        assert_eq!(inner.parked_count(), 1);
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
        assert_eq!(inner.parked_count(), 1);
        assert!(!flag.0.load(Ordering::SeqCst));
    }
}
