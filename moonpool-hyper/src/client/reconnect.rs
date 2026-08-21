//! A channel that owns its connection and rebuilds it after a loss.

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use hyper::body::{Body, Incoming};
use hyper::rt::bounds::Http2ClientConnExec;
use hyper::{Request, Response};
use moonpool_core::{Detach, NetworkProvider, Providers, TaskProvider, TimeProvider};

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
    /// Delay before the first retry. Doubles per consecutive failure.
    pub initial_reconnect_delay: Duration,

    /// Ceiling for the doubling backoff.
    pub max_reconnect_delay: Duration,

    /// Budget for one attempt, covering the TCP connect and the h2 handshake.
    pub connect_timeout: Duration,

    /// Consecutive failures tolerated before the channel gives up for good.
    /// `None`, the default, retries forever.
    pub max_connection_failures: Option<u32>,

    /// h2 PING keepalive. `None`, the default, is hyper's own default: no
    /// keepalive, so a connection to a peer that goes silent without closing
    /// the socket is only detected by a request failing.
    pub keep_alive: Option<KeepAlive>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            max_connection_failures: None,
            keep_alive: None,
        }
    }
}

impl ChannelConfig {
    /// How long to wait before the attempt that follows `failures`
    /// consecutive failures.
    ///
    /// Plain doubling from `initial_reconnect_delay`, saturating at
    /// `max_reconnect_delay`, with no jitter: production jitter exists to
    /// desynchronize a fleet of clients, and here it would only make the
    /// simulation's reconnect timing depend on something other than the seed.
    fn backoff_delay(&self, failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        let mut delay = self.initial_reconnect_delay;
        // Loop count is bounded by the ceiling, not by `failures`.
        for _ in 1..failures {
            if delay >= self.max_reconnect_delay {
                break;
            }
            delay = delay.saturating_mul(2);
        }
        delay.min(self.max_reconnect_delay)
    }
}

/// A tower [`Service`](tower_service::Service) that keeps an h2 connection to
/// one destination, reconnecting as needed.
///
/// This plays the role `tonic::transport::Channel` plays in production: hand it
/// to a generated gRPC client (or any tower stack) and it connects on the first
/// use, serves requests over the live connection, and rebuilds the connection
/// after a loss with deterministic backoff. Cloning shares one connection and
/// one reconnection state machine.
///
/// # Readiness and errors
///
/// `poll_ready` drives everything. It returns `Ready(Ok)` only with a live
/// connection, `Pending` while one is being established (the caller is woken in
/// registration order), and `Ready(Err)` once per failed attempt: the failure is
/// reported to a single caller and the next poll starts a fresh attempt. The
/// channel never retries a *request* on the caller's behalf, which is the
/// correct gRPC semantic (the caller knows whether its RPC is idempotent).
///
/// # Where it may be polled
///
/// `poll_ready` spawns the connection task through the task provider, so the
/// channel must be polled from inside a provider task, exactly like calling
/// `tokio::spawn` requires a tokio runtime.
///
/// One task exists per connection: it waits out the backoff, connects, then
/// drives the connection and exits when the connection ends. A channel that
/// nobody polls starts nothing, and with the default `keep_alive` of `None` an
/// established connection generates no timer traffic of its own, so a quiesced
/// simulation stays quiesced. Note that dropping every clone does not close a
/// live connection: the task keeps driving it (keepalive pings included, when
/// configured) until the peer closes it or the process goes away.
pub struct ReconnectingChannel<P: Providers, B> {
    shared: Arc<Shared<P, B>>,
}

// Manual: the derive would demand `B: Clone`, and cloning a channel shares
// state rather than duplicating it.
impl<P: Providers, B> Clone for ReconnectingChannel<P, B> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<P: Providers, B> std::fmt::Debug for ReconnectingChannel<P, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconnectingChannel")
            .field("destination", &self.shared.destination)
            .finish_non_exhaustive()
    }
}

/// State shared by every clone of a channel and by the connection task.
struct Shared<P: Providers, B> {
    providers: P,
    destination: String,
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
/// connection task reads what it needs, releases, and only takes the lock again
/// to publish an outcome.
struct Inner<B> {
    conn: Conn<B>,

    /// Consecutive failed attempts, reset by a successful handshake. Drives
    /// both the backoff and `max_connection_failures`.
    failures: u32,

    /// Bumped on every transition that retires a connection task. A task only
    /// writes back if the generation it was spawned with still stands, which
    /// keeps a task whose connection died late from clobbering the state of the
    /// task that replaced it.
    generation: u64,

    /// Set by a failed attempt, taken by the next `poll_ready`.
    last_error: Option<ChannelError>,

    /// Callers parked in `poll_ready`, woken in registration order so wake
    /// ordering is a function of poll order alone.
    wakers: Vec<Waker>,
}

/// Connection state.
enum Conn<B> {
    /// No connection and no attempt running.
    Disconnected,
    /// A connection task is running: connecting, or waiting out backoff.
    Connecting,
    /// A live connection. Requests are served by cloning this channel.
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
    /// Create a channel to `destination`. No connection is made until the
    /// channel is first polled ready.
    pub fn new(providers: P, destination: impl Into<String>, config: ChannelConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                providers,
                destination: destination.into(),
                config,
                inner: Mutex::new(Inner {
                    conn: Conn::Disconnected,
                    failures: 0,
                    generation: 0,
                    last_error: None,
                    wakers: Vec::new(),
                }),
            }),
        }
    }

    fn poll_connection(&self, cx: &mut Context<'_>) -> Poll<Result<(), ChannelError>> {
        // Everything that touches the state happens in here; the guard is gone
        // before anything is spawned, so the channel lock is never held while
        // the task provider takes its own.
        let attempt = {
            let mut inner = self.shared.lock();

            // A failed attempt is reported to exactly one caller; everyone else
            // (and this caller's next poll) goes on to start a new attempt.
            if let Some(error) = inner.last_error.take() {
                return Poll::Ready(Err(error));
            }

            // A connection that died since the last poll is demoted here rather
            // than waited on, so the reconnect starts in this very poll.
            if matches!(&inner.conn, Conn::Connected(c) if c.is_closed()) {
                inner.generation = inner.generation.wrapping_add(1);
                inner.conn = Conn::Disconnected;
            } else if matches!(inner.conn, Conn::Connected(_)) {
                return Poll::Ready(Ok(()));
            }

            let mut attempt = None;
            if matches!(inner.conn, Conn::Disconnected) {
                if let Some(max) = self.shared.config.max_connection_failures
                    && inner.failures >= max
                {
                    return Poll::Ready(Err(ChannelError::GaveUp {
                        destination: self.shared.destination.clone(),
                        failures: inner.failures,
                    }));
                }

                // Single flight: flipping to Connecting under the lock means
                // the next caller to arrive parks instead of starting a second
                // attempt.
                inner.generation = inner.generation.wrapping_add(1);
                inner.conn = Conn::Connecting;
                attempt = Some(inner.generation);
            }

            if !inner.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                inner.wakers.push(cx.waker().clone());
            }
            attempt
        };

        if let Some(generation) = attempt {
            let shared = Arc::clone(&self.shared);
            self.shared
                .providers
                .task()
                .spawn_task("h2-channel", connect_and_serve(shared, generation))
                .detach();
        }
        Poll::Pending
    }
}

impl<P: Providers, B> ReconnectingChannel<P, B> {
    /// The address this channel connects to.
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.shared.destination
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
        // The request takes its own clone of the sender and lives or dies with
        // it: a reconnection underneath does not rescue it, and its failure
        // does not disturb the channel's state.
        let connected = match &self.shared.lock().conn {
            Conn::Connected(channel) => Some(channel.clone()),
            Conn::Disconnected | Conn::Connecting => None,
        };

        match connected {
            Some(mut channel) => tower_service::Service::call(&mut channel, req),
            None => Box::pin(std::future::ready(Err(ChannelError::NotReady))),
        }
    }
}

/// Connect (after any backoff), publish the connection, then drive it until it
/// ends. Runs as one provider task per connection attempt.
#[tracing::instrument(
    name = "h2_connect",
    skip_all,
    fields(destination = %shared.destination, generation)
)]
async fn connect_and_serve<P, B>(shared: Arc<Shared<P, B>>, generation: u64)
where
    P: Providers,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
    HyperExecutor<P::Task>: Http2ClientConnExec<B, ChannelIo<P>>,
{
    let destination = shared.destination.clone();
    let delay = shared.config.backoff_delay(shared.lock().failures);

    tracing::info!(
        destination = %destination,
        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        "h2_channel_connecting"
    );

    // Backoff is served here, never in the caller: a caller that stops polling
    // must not leave the schedule half-applied.
    if !delay.is_zero() && shared.providers.time().sleep(delay).await.is_err() {
        record_failure(
            &shared,
            generation,
            ChannelError::Connect {
                destination,
                reason: "time provider stopped".to_owned(),
            },
        );
        return;
    }

    let attempt = async {
        let stream = shared
            .providers
            .network()
            .connect(&shared.destination)
            .await
            .map_err(|e| ChannelError::Connect {
                destination: shared.destination.clone(),
                reason: e.to_string(),
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

        builder
            .handshake(HyperIo::new(stream))
            .await
            .map_err(|e| ChannelError::Connect {
                destination: shared.destination.clone(),
                reason: e.to_string(),
            })
    };

    // One budget for connect plus handshake: a peer that accepts the TCP
    // connection and then goes silent must not hold the channel forever.
    let outcome = shared
        .providers
        .time()
        .timeout(shared.config.connect_timeout, attempt)
        .await;

    let (sender, connection) = match outcome {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            tracing::info!(destination = %destination, error = %error, "h2_channel_connect_failed");
            record_failure(&shared, generation, error);
            return;
        }
        Err(_elapsed) => {
            let error = ChannelError::Connect {
                destination: destination.clone(),
                reason: "connect timed out".to_owned(),
            };
            tracing::info!(destination = %destination, error = %error, "h2_channel_connect_failed");
            record_failure(&shared, generation, error);
            return;
        }
    };

    let parked = {
        let mut inner = shared.lock();
        if inner.generation != generation {
            // Retired while connecting: drop the connection, which closes the
            // stream, and leave the current task's state alone.
            return;
        }
        inner.conn = Conn::Connected(H2Channel::new(sender));
        inner.failures = 0;
        take_wakers(&mut inner)
    };
    wake_all(parked);
    tracing::info!(destination = %destination, "h2_channel_connected");

    // Requests only make progress while this future is polled, so the task
    // stays here for the life of the connection and exits when it ends.
    match connection.await {
        Ok(()) => tracing::info!(destination = %destination, "h2_channel_closed"),
        Err(error) => {
            tracing::info!(destination = %destination, error = %error, "h2_channel_closed");
        }
    }

    let parked = {
        let mut inner = shared.lock();
        if inner.generation != generation {
            return;
        }
        inner.conn = Conn::Disconnected;
        // No stored error: a connection that ends is not a failed attempt, so
        // it neither counts toward the failure budget nor delays the next
        // connect. The next poll_ready reconnects immediately.
        take_wakers(&mut inner)
    };
    wake_all(parked);
}

/// Record a failed attempt and hand the error to the waiting callers.
fn record_failure<P: Providers, B>(shared: &Shared<P, B>, generation: u64, error: ChannelError) {
    let parked = {
        let mut inner = shared.lock();
        if inner.generation != generation {
            return;
        }
        inner.failures = inner.failures.saturating_add(1);
        inner.conn = Conn::Disconnected;
        inner.last_error = Some(error);
        take_wakers(&mut inner)
    };
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
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::Full;
    use moonpool_core::TokioProviders;
    use tower_service::Service as _;

    use super::{ChannelConfig, ReconnectingChannel};
    use crate::ChannelError;

    /// The channel a production tokio stack would build. Naming this type at
    /// all is the point: it makes the compiler discharge hyper's sealed
    /// `Http2ClientConnExec` bound for `HyperExecutor<TokioTaskProvider>` over
    /// a real provider stream, which no generic definition can prove on its
    /// own.
    type TestChannel = ReconnectingChannel<TokioProviders, Full<Bytes>>;

    #[test]
    fn call_before_ready_fails_instead_of_panicking() {
        let mut channel: TestChannel = ReconnectingChannel::new(
            TokioProviders::new(),
            "10.0.0.1:50051",
            ChannelConfig::default(),
        );
        assert_eq!(channel.destination(), "10.0.0.1:50051");
        assert!(!channel.is_connected());

        // No poll_ready first, so no connection and no task provider involved:
        // the request must resolve to NotReady rather than panic or hang.
        let request = hyper::Request::new(Full::new(Bytes::from_static(b"body")));
        let response = futures::executor::block_on(channel.call(request));
        assert!(matches!(response, Err(ChannelError::NotReady)));
    }

    #[test]
    fn clones_share_one_connection_state() {
        let channel: TestChannel = ReconnectingChannel::new(
            TokioProviders::new(),
            "10.0.0.2:50051",
            ChannelConfig::default(),
        );
        let clone = channel.clone();
        assert!(Arc::ptr_eq(&channel.shared, &clone.shared));
    }

    #[test]
    fn defaults_mirror_the_peer_config() {
        let config = ChannelConfig::default();
        assert_eq!(config.initial_reconnect_delay, Duration::from_millis(100));
        assert_eq!(config.max_reconnect_delay, Duration::from_secs(30));
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert!(config.max_connection_failures.is_none());
        assert!(config.keep_alive.is_none());
    }

    #[test]
    fn the_first_attempt_does_not_wait() {
        assert_eq!(ChannelConfig::default().backoff_delay(0), Duration::ZERO);
    }

    #[test]
    fn backoff_doubles_per_failure() {
        let config = ChannelConfig::default();
        assert_eq!(config.backoff_delay(1), Duration::from_millis(100));
        assert_eq!(config.backoff_delay(2), Duration::from_millis(200));
        assert_eq!(config.backoff_delay(3), Duration::from_millis(400));
        assert_eq!(config.backoff_delay(4), Duration::from_millis(800));
    }

    #[test]
    fn backoff_saturates_at_the_ceiling() {
        let config = ChannelConfig::default();
        // 100ms doubled nine times is 25.6s, ten times would overshoot 30s.
        assert_eq!(config.backoff_delay(9), Duration::from_millis(25_600));
        assert_eq!(config.backoff_delay(10), Duration::from_secs(30));
        assert_eq!(config.backoff_delay(1_000), Duration::from_secs(30));
        assert_eq!(config.backoff_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn an_initial_delay_above_the_ceiling_is_clamped() {
        let config = ChannelConfig {
            initial_reconnect_delay: Duration::from_secs(45),
            max_reconnect_delay: Duration::from_secs(30),
            ..ChannelConfig::default()
        };
        assert_eq!(config.backoff_delay(1), Duration::from_secs(30));
        assert_eq!(config.backoff_delay(5), Duration::from_secs(30));
    }
}
