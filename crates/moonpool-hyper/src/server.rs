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

use futures::future::{Either, select};
use futures::io::{AsyncRead, AsyncWrite};
use hyper::body::{Body, Incoming};
use hyper::rt::bounds::Http2ServerConnExec;
use hyper::server::conn::http2;
use hyper::{Request, Response};
use moonpool_core::Providers;
use tracing::instrument;

use crate::config::KeepAlive;
use crate::io::HyperIo;
use crate::rt::{HyperExecutor, HyperTimer};
use crate::service::{TowerToHyperService, TowerToHyperServiceFuture};

/// How an [`H2Server`] serves each connection.
#[derive(Clone, Debug, Default)]
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
            config: H2ServerConfig::default(),
        }
    }

    /// Replace the configuration.
    #[must_use]
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
    /// The future resolves to [`hyper::Error`] when the connection ends badly:
    /// an IO failure, a protocol error, or a client that disappears. Under
    /// simulated chaos that is an expected outcome, not a bug.
    pub fn serve_connection<S, Svc, B>(
        &self,
        stream: S,
        service: Svc,
    ) -> impl Future<Output = Result<(), hyper::Error>> + Send + 'static
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
        let connection = self.connection(stream, service);
        async move {
            let result = connection.await;
            report(&result, false);
            result
        }
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
    /// # Errors
    ///
    /// Same as [`serve_connection`](Self::serve_connection): the connection's
    /// own [`hyper::Error`], whether or not a shutdown was requested. A drain
    /// that cannot complete (the peer stops reading) surfaces here too.
    pub fn serve_connection_with_shutdown<S, Svc, B, F>(
        &self,
        stream: S,
        service: Svc,
        shutdown: F,
    ) -> impl Future<Output = Result<(), hyper::Error>> + Send + 'static
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
        let connection = self.connection(stream, service);
        async move {
            let connection = std::pin::pin!(connection);
            let shutdown = std::pin::pin!(shutdown);

            // `futures::future::select` rather than moonpool's select!, which
            // is tokio's macro: this is library code with two branches and no
            // need for the seeded branch rotation.
            match select(connection, shutdown).await {
                Either::Left((result, _shutdown)) => {
                    report(&result, false);
                    result
                }
                Either::Right(((), mut connection)) => {
                    connection.as_mut().graceful_shutdown();
                    let result = connection.await;
                    report(&result, true);
                    result
                }
            }
        }
    }

    /// Bind a stream and a service into a hyper connection.
    fn connection<S, Svc, B>(
        &self,
        stream: S,
        service: Svc,
    ) -> http2::Connection<HyperIo<S>, TowerToHyperService<Svc>, HyperExecutor<P::Task>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        Svc: tower_service::Service<Request<Incoming>, Response = Response<B>> + Clone,
        Svc::Error: Into<Box<dyn Error + Send + Sync>>,
        B: Body + 'static,
        B::Error: Into<Box<dyn Error + Send + Sync>>,
        HyperExecutor<P::Task>:
            Http2ServerConnExec<TowerToHyperServiceFuture<Svc, Request<Incoming>>, B>,
    {
        let io = HyperIo::new(stream).with_vectored_writes(self.config.vectored_writes);
        self.builder()
            .serve_connection(io, TowerToHyperService::new(service))
    }
}

/// One event per connection, so a busy server does not drown the trace.
fn report(result: &Result<(), hyper::Error>, graceful: bool) {
    match result {
        Ok(()) => tracing::info!(graceful, outcome = "ok", "h2_server_connection_finished"),
        Err(error) => tracing::info!(
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
        });

        assert!(server.config().vectored_writes);
        assert!(server.config().keep_alive.is_some());
        // Cloning carries the configuration.
        assert!(server.clone().config().vectored_writes);
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
