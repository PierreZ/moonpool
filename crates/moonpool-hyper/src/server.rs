//! Serve h2 connections on the provider runtime.
//!
//! [`H2Server`] bundles the three things every hyper server connection needs
//! from a runtime (an executor for per-request tasks, a timer for keepalive,
//! and IO) and hands back a `Send + 'static` future per accepted connection,
//! ready to spawn as a provider task.
//!
//! Deliberately per-connection rather than an accept loop: the race between
//! accepting and serving belongs to the caller's `select!`, where the
//! simulation's seeded scheduling can explore it, and an HTTP/1 server (axum)
//! has to drive its `!Send` connection inline anyway.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use futures::future::{Either, select};
use futures::io::{AsyncRead, AsyncWrite};
use hyper::body::{Body, Incoming};
use hyper::rt::bounds::Http2ServerConnExec;
use hyper::server::conn::http2;
use hyper::{Request, Response};
use moonpool_core::{Providers, TimeProvider};
use tracing::instrument;

use crate::config::KeepAlive;
use crate::io::HyperIo;
use crate::rt::{HyperExecutor, HyperTimer};
use crate::service::{TowerToHyperService, TowerToHyperServiceFuture};

/// The default [`H2ServerConfig::drain_timeout`]: long enough for any
/// ordinary in-flight request, short enough that a stuck peer cannot hold a
/// shutdown forever.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How an [`H2Server`] serves each connection.
#[derive(Clone, Debug)]
pub struct H2ServerConfig {
    /// h2 PING keepalive.
    ///
    /// `None`, the default, is hyper's own default: no keepalive, so a client
    /// that vanishes without closing the socket holds the connection until the
    /// transport notices. Note that hyper's server takes only the interval and
    /// the timeout: [`KeepAlive::while_idle`] is a client-side knob and is
    /// ignored here.
    pub keep_alive: Option<KeepAlive>,

    /// Whether the accepted stream claims efficient vectored writes.
    ///
    /// Passed to [`HyperIo::with_vectored_writes`]. Default `false`, matching
    /// `HyperIo`'s own default: futures-io cannot be asked whether the
    /// underlying stream really implements `poll_write_vectored`.
    pub vectored_writes: bool,

    /// How long a graceful drain may take once shutdown is requested.
    ///
    /// When the in-flight streams have not finished by then (the peer stopped
    /// reading, a partition, a clogged link), the connection is dropped —
    /// resetting what is left — and
    /// [`serve_connection_with_shutdown`](H2Server::serve_connection_with_shutdown)
    /// resolves to [`ServeError::DrainTimedOut`]. `None` waits for the drain
    /// without a bound, which is only safe where something else (such as the
    /// simulation's process grace period) bounds it. Defaults to
    /// [`DEFAULT_DRAIN_TIMEOUT`].
    pub drain_timeout: Option<Duration>,
}

impl Default for H2ServerConfig {
    fn default() -> Self {
        Self {
            keep_alive: None,
            vectored_writes: false,
            drain_timeout: Some(DEFAULT_DRAIN_TIMEOUT),
        }
    }
}

/// Why a served connection ended badly.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The connection itself failed: an IO failure, a protocol error, or a
    /// client that disappeared. Under simulated chaos that is an expected
    /// outcome, not a bug.
    #[error(transparent)]
    Connection(#[from] hyper::Error),

    /// Shutdown was requested and the graceful drain did not finish within
    /// [`H2ServerConfig::drain_timeout`], so the connection was dropped with
    /// streams still open.
    #[error("graceful drain did not finish within {deadline:?}")]
    DrainTimedOut {
        /// The drain deadline that ran out.
        deadline: Duration,
    },
}

/// Serves h2 connections over the provider traits.
///
/// Build one per process and reuse it for every accepted connection: it is
/// cheap to clone and holds no connection state of its own.
///
/// ```text
/// loop {
///     select! {
///         accepted = listener.accept() => {
///             let (stream, _addr) = accepted?;
///             let conn = server.serve_connection_with_shutdown(
///                 stream,
///                 service.clone(),
///                 ctx.shutdown().cancelled_owned(),
///             );
///             ctx.task().spawn_task("h2-conn", conn).detach();
///         }
///         () = ctx.shutdown().cancelled() => return Ok(()),
///     }
/// }
/// ```
#[derive(Clone)]
pub struct H2Server<P: Providers> {
    executor: HyperExecutor<P::Task>,
    timer: HyperTimer<P::Time>,
    time: P::Time,
    config: H2ServerConfig,
}

impl<P: Providers> fmt::Debug for H2Server<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("H2Server")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<P: Providers> H2Server<P> {
    /// Create a server that spawns and sleeps through the given providers.
    #[instrument(skip_all)]
    pub fn new(providers: &P) -> Self {
        Self {
            executor: HyperExecutor::new(providers.task().clone()),
            timer: HyperTimer::new(providers.time().clone()),
            time: providers.time().clone(),
            config: H2ServerConfig::default(),
        }
    }

    /// Replace the configuration.
    #[must_use]
    #[instrument(level = "debug", skip_all)]
    pub fn with_config(mut self, config: H2ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// The configuration in force.
    #[must_use]
    pub fn config(&self) -> &H2ServerConfig {
        &self.config
    }

    /// A hyper connection builder pre-wired with this server's executor, timer
    /// and keepalive settings.
    ///
    /// The escape hatch for the many h2 knobs this type does not expose (window
    /// sizes, concurrent stream limits, `enable_connect_protocol`). The caller
    /// takes over from there, including wrapping the stream in
    /// [`HyperIo`] and the service in [`TowerToHyperService`], which is what
    /// [`serve_connection`](Self::serve_connection) would have done.
    ///
    /// Hyper's automatic `Date` response header is disabled because it reads
    /// the system clock outside [`HyperTimer`], making otherwise identical
    /// simulations produce different HPACK bytes. A production caller that
    /// needs the header can re-enable it on the returned builder or add it in
    /// its service.
    #[must_use]
    pub fn builder(&self) -> http2::Builder<HyperExecutor<P::Task>> {
        let mut builder = http2::Builder::new(self.executor.clone());
        builder.timer(self.timer.clone());
        builder.auto_date_header(false);
        if let Some(keep_alive) = &self.config.keep_alive {
            builder
                .keep_alive_interval(keep_alive.interval)
                .keep_alive_timeout(keep_alive.timeout);
        }
        builder
    }

    /// Serve one accepted connection.
    ///
    /// Takes a tower service (the shape tonic's generated servers and axum's
    /// routers have) and adapts it internally. The returned future owns
    /// everything it needs, so spawn it as a provider task; nothing on the
    /// connection progresses until it is polled.
    ///
    /// # Errors
    ///
    /// The future resolves to [`ServeError::Connection`] when the connection
    /// ends badly: an IO failure, a protocol error, or a client that
    /// disappears. Under simulated chaos that is an expected outcome, not a
    /// bug.
    pub fn serve_connection<S, Svc, B>(
        &self,
        stream: S,
        service: Svc,
    ) -> impl Future<Output = Result<(), ServeError>> + Send + 'static
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        Svc: tower_service::Service<Request<Incoming>, Response = Response<B>>
            + Clone
            + Send
            + 'static,
        Svc::Error: Into<Box<dyn Error + Send + Sync>>,
        Svc::Future: Send,
        B: Body + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        HyperExecutor<P::Task>:
            Http2ServerConnExec<TowerToHyperServiceFuture<Svc, Request<Incoming>>, B>,
    {
        self.serve_connection_with_shutdown(stream, service, std::future::pending())
    }

    /// Serve one accepted connection, draining it when `shutdown` resolves.
    ///
    /// On shutdown the connection stops accepting new streams and the in-flight
    /// ones are allowed to finish (hyper's
    /// [`graceful_shutdown`](http2::Connection::graceful_shutdown)), so a
    /// process rebooting on a signal answers the requests it already took
    /// instead of resetting them. The future then resolves with the
    /// connection's own result.
    ///
    /// The drain is bounded by [`H2ServerConfig::drain_timeout`]: a peer that
    /// stops reading cannot hold the future open forever.
    ///
    /// # Errors
    ///
    /// Same as [`serve_connection`](Self::serve_connection): the connection's
    /// own error, whether or not a shutdown was requested. A drain that does
    /// not complete within the deadline resolves to
    /// [`ServeError::DrainTimedOut`].
    pub fn serve_connection_with_shutdown<S, Svc, B, F>(
        &self,
        stream: S,
        service: Svc,
        shutdown: F,
    ) -> impl Future<Output = Result<(), ServeError>> + Send + 'static
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        Svc: tower_service::Service<Request<Incoming>, Response = Response<B>>
            + Clone
            + Send
            + 'static,
        Svc::Error: Into<Box<dyn Error + Send + Sync>>,
        Svc::Future: Send,
        B: Body + Send + 'static,
        B::Data: Send,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        F: Future<Output = ()> + Send + 'static,
        HyperExecutor<P::Task>:
            Http2ServerConnExec<TowerToHyperServiceFuture<Svc, Request<Incoming>>, B>,
    {
        let io = HyperIo::new(stream).with_vectored_writes(self.config.vectored_writes);
        let connection = self
            .builder()
            .serve_connection(io, TowerToHyperService::new(service));
        let time = self.time.clone();
        let drain_timeout = self.config.drain_timeout;
        async move {
            let connection = std::pin::pin!(connection);
            let shutdown = std::pin::pin!(shutdown);

            // `futures::future::select` rather than moonpool's select!, which
            // is tokio's macro: this is library code with two branches and no
            // need for the seeded branch rotation.
            match select(connection, shutdown).await {
                Either::Left((result, _shutdown)) => {
                    let result = result.map_err(ServeError::from);
                    report(&result, false);
                    result
                }
                Either::Right(((), mut connection)) => {
                    connection.as_mut().graceful_shutdown();
                    let result = match drain_timeout {
                        None => connection.await.map_err(ServeError::from),
                        // Dropping the connection on expiry closes the stream
                        // and resets whatever the drain had left.
                        Some(deadline) => match time.timeout(deadline, connection).await {
                            Ok(result) => result.map_err(ServeError::from),
                            Err(_) => Err(ServeError::DrainTimedOut { deadline }),
                        },
                    };
                    report(&result, true);
                    result
                }
            }
        }
    }
}

/// One event per connection, so a busy server does not drown the trace.
fn report(result: &Result<(), ServeError>, graceful: bool) {
    match result {
        Ok(()) => tracing::info!(graceful, outcome = "ok", "h2_server_connection_finished"),
        Err(error @ ServeError::DrainTimedOut { .. }) => tracing::info!(
            graceful,
            outcome = "drain_timed_out",
            detail = %error,
            "h2_server_connection_finished"
        ),
        Err(error @ ServeError::Connection(_)) => tracing::info!(
            graceful,
            outcome = "error",
            detail = %error,
            "h2_server_connection_finished"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use bytes::Bytes;
    use futures::io::Cursor;
    use http_body_util::Full;
    use moonpool_core::TokioProviders;

    use super::{H2Server, H2ServerConfig};
    use crate::config::KeepAlive;
    use hyper::body::Incoming;
    use hyper::{Request, Response};
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// A minimal always-ready tower service, the shape a generated gRPC server
    /// or an axum router has.
    #[derive(Clone)]
    struct Answer;

    impl tower_service::Service<Request<Incoming>> for Answer {
        type Response = Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Incoming>) -> Self::Future {
            std::future::ready(Ok(Response::new(Full::new(Bytes::from_static(b"ok")))))
        }
    }

    fn assert_send_static<T: Send + 'static>(_value: &T) {}

    /// The bound proof. Naming these concrete types is the point: it makes the
    /// compiler discharge hyper's sealed `Http2ServerConnExec` for
    /// `HyperExecutor<TokioTaskProvider>` and prove the connection future
    /// `Send + 'static`, which no generic definition can prove on its own.
    ///
    /// The futures are built and dropped, never polled: constructing a hyper
    /// connection touches neither the IO nor the executor, so no runtime is
    /// needed here.
    #[test]
    fn serve_futures_are_send_and_static_over_real_providers() {
        let server = H2Server::new(&TokioProviders::new());

        let plain = server.serve_connection(Cursor::new(Vec::new()), Answer);
        assert_send_static(&plain);
        drop(plain);

        let with_shutdown = server.serve_connection_with_shutdown(
            Cursor::new(Vec::new()),
            Answer,
            std::future::ready(()),
        );
        assert_send_static(&with_shutdown);
        drop(with_shutdown);
    }

    #[test]
    fn config_defaults_to_no_keepalive_and_no_vectored_writes() {
        let config = H2ServerConfig::default();
        assert!(config.keep_alive.is_none());
        assert!(!config.vectored_writes);
        assert_eq!(config.drain_timeout, Some(super::DEFAULT_DRAIN_TIMEOUT));
    }

    #[test]
    fn with_config_replaces_the_configuration() {
        let server = H2Server::new(&TokioProviders::new()).with_config(H2ServerConfig {
            keep_alive: Some(KeepAlive {
                interval: Duration::from_secs(3),
                timeout: Duration::from_secs(2),
                while_idle: true,
            }),
            vectored_writes: true,
            drain_timeout: None,
        });

        assert!(server.config().vectored_writes);
        assert!(server.config().keep_alive.is_some());
        // Cloning carries the configuration.
        assert!(server.clone().config().vectored_writes);
    }

    /// A service that takes the request and never answers: the stream stays
    /// in flight for as long as the connection lives.
    #[derive(Clone)]
    struct NeverAnswers;

    impl tower_service::Service<Request<Incoming>> for NeverAnswers {
        type Response = Response<Full<Bytes>>;
        type Error = Infallible;
        type Future = std::future::Pending<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Incoming>) -> Self::Future {
            std::future::pending()
        }
    }

    /// A client that opens one request stream and then goes silent: it sends
    /// the preface, an empty SETTINGS frame and a `GET /` HEADERS frame, then
    /// never sends another byte (not even a SETTINGS ack) nor closes. Writes
    /// are swallowed.
    struct SilentClient {
        script: Cursor<Vec<u8>>,
    }

    impl SilentClient {
        fn new() -> Self {
            let mut script = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
            // SETTINGS: length 0, type 0x4, no flags, stream 0.
            script.extend_from_slice(&[0, 0, 0, 0x4, 0, 0, 0, 0, 0]);
            // HEADERS: length 3, type 0x1, END_STREAM | END_HEADERS, stream 1,
            // then the HPACK static-table entries :method GET, :scheme http,
            // :path /.
            script.extend_from_slice(&[0, 0, 3, 0x1, 0x5, 0, 0, 0, 1, 0x82, 0x86, 0x84]);
            Self {
                script: Cursor::new(script),
            }
        }
    }

    impl futures::io::AsyncRead for SilentClient {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            match std::pin::Pin::new(&mut self.script).poll_read(cx, buf) {
                // The script ran out: the client stays connected and silent.
                Poll::Ready(Ok(0)) => Poll::Pending,
                other => other,
            }
        }
    }

    impl futures::io::AsyncWrite for SilentClient {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A graceful drain that cannot finish (a stream is still in flight and
    /// the peer is silent) resolves once the drain deadline runs out, instead
    /// of holding the connection future open forever.
    #[tokio::test]
    async fn a_stuck_drain_times_out() {
        let deadline = Duration::from_millis(50);
        let server = H2Server::new(&TokioProviders::new()).with_config(H2ServerConfig {
            drain_timeout: Some(deadline),
            ..H2ServerConfig::default()
        });
        let connection = server.serve_connection_with_shutdown(
            SilentClient::new(),
            NeverAnswers,
            tokio::time::sleep(Duration::from_millis(20)),
        );

        let outcome = tokio::time::timeout(Duration::from_secs(5), connection)
            .await
            .expect("the drain deadline must bound the connection future");
        assert!(
            matches!(outcome, Err(super::ServeError::DrainTimedOut { deadline: d }) if d == deadline),
            "expected a drain timeout, got {outcome:?}"
        );
    }

    #[test]
    fn builder_is_available_as_an_escape_hatch() {
        let server: H2Server<TokioProviders> = H2Server::new(&TokioProviders::new());
        let builder = server.builder();
        // Pre-wired and reusable: hyper's builder is Clone, so a caller can
        // tune one and serve many connections from it.
        drop(builder.clone());
        drop(builder);
    }
}
