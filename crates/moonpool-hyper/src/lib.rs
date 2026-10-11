//! # moonpool-hyper
//!
//! hyper 1.x integration for the moonpool provider traits.
//!
//! hyper is runtime-agnostic by design, but every adapter it ships with
//! (`hyper_util`'s `TokioExecutor`, `TokioTimer`, `TokioIo`) is
//! tokio-specific. This crate gives hyper the same hooks over moonpool's
//! provider traits. An HTTP/2 stack (tonic gRPC, axum, plain hyper) then
//! runs unchanged on `TokioProviders` in production and on the simulation
//! providers under test. The crate brings no providers and no tokio runtime
//! of its own.
//!
//! ## Quick example
//!
//! The same server and client code, generic over a
//! [`Providers`](moonpool_core::Providers) bundle. Any tower service works:
//! a tonic generated server, an axum router, or the small one below.
//!
//! ```rust
//! # #[cfg(all(feature = "client", feature = "server"))] mod demo {
//! use std::convert::Infallible;
//! use std::future::{Ready, ready};
//! use std::task::{Context, Poll};
//!
//! use hyper::body::Incoming;
//! use hyper::{Request, Response};
//! use moonpool_core::{Detach, NetworkProvider, Providers, TaskProvider, TcpListenerTrait};
//! use moonpool_hyper::{ChannelConfig, H2Server, ReconnectingChannel};
//!
//! /// A tower service that answers every request with "pong".
//! #[derive(Clone)]
//! struct Pong;
//!
//! impl tower_service::Service<Request<Incoming>> for Pong {
//!     type Response = Response<String>;
//!     type Error = Infallible;
//!     type Future = Ready<Result<Self::Response, Infallible>>;
//!
//!     fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
//!         Poll::Ready(Ok(()))
//!     }
//!
//!     fn call(&mut self, _request: Request<Incoming>) -> Self::Future {
//!         ready(Ok(Response::new("pong".to_string())))
//!     }
//! }
//!
//! /// Accept connections and serve each one as a provider task.
//! pub async fn serve<P: Providers>(providers: P, addr: &str) -> std::io::Result<()> {
//!     let listener = providers.network().bind(addr).await?;
//!     let server = H2Server::new(&providers);
//!     loop {
//!         let (stream, _peer) = listener.accept().await?;
//!         let connection = server.serve_connection(stream, Pong);
//!         providers
//!             .task()
//!             .spawn_task("h2-connection", async move {
//!                 // A connection that ends badly is normal under chaos.
//!                 let _ = connection.await;
//!             })
//!             .detach();
//!     }
//! }
//!
//! /// One cloneable h2 channel; it dials lazily and reconnects with backoff.
//! /// It is a tower service, so tonic clients take it as their transport.
//! pub fn channel<P: Providers>(providers: &P, addr: &str) -> ReconnectingChannel<P, String> {
//!     ReconnectingChannel::new(providers, addr, ChannelConfig::default())
//! }
//! # }
//! ```
//!
//! ## Features
//!
//! | Type | hyper hook | Provider |
//! |------|------------|----------|
//! | [`HyperExecutor`] | [`hyper::rt::Executor`] | [`TaskProvider`](moonpool_core::TaskProvider) |
//! | [`HyperTimer`] | [`hyper::rt::Timer`] | [`TimeProvider`](moonpool_core::TimeProvider) |
//! | [`HyperIo`] | [`hyper::rt::Read`] + [`hyper::rt::Write`] | any [`NetworkProvider`](moonpool_core::NetworkProvider) stream |
//! | [`TowerToHyperService`] | [`hyper::service::Service`] | (adapts a tower service) |
//!
//! On top of these hooks:
//!
//! - **[`H2Server`]** (feature `server`): serves one accepted connection
//!   with a tower service. [`H2Server::serve_connection_with_shutdown`] drains
//!   in-flight streams on a shutdown signal, bounded by
//!   [`H2ServerConfig::drain_timeout`] (default [`DEFAULT_DRAIN_TIMEOUT`], 30
//!   seconds). [`H2Server::builder`] returns a pre-wired hyper builder for the
//!   h2 knobs this crate does not expose.
//! - **[`ReconnectingChannel`]** (feature `client`): a cloneable h2 channel
//!   with lazy connect, exponential reconnect backoff, a connection timeout,
//!   an optional failure budget ([`ChannelConfig`]) and an explicit, shared
//!   [`close`](ReconnectingChannel::close). [`H2Channel`] wraps one hyper `SendRequest` as a tower service.
//!   Failures are [`ChannelError`].
//! - **[`KeepAlive`]**: the h2 PING settings that both sides accept.
//!
//! [`HyperIo`] replaces the `tokio_util::compat::Compat` plus
//! `hyper_util::rt::TokioIo` two-hop bridge. Every moonpool stream already
//! implements the futures-io `AsyncRead` and `AsyncWrite` pair, and that is
//! all hyper's IO traits need.
//!
//! ## Feature flags
//!
//! | Feature | Default | What it adds |
//! |---|---|---|
//! | `client` | on | `ReconnectingChannel`, `H2Channel`, `ChannelConfig`, `ChannelError`, `ResponseFuture` (hyper `client` and `http2`) |
//! | `server` | on | `H2Server`, `H2ServerConfig`, `ServeError`, `DEFAULT_DRAIN_TIMEOUT` (hyper `server` and `http2`) |
//!
//! With `default-features = false`, the runtime adapters, the IO bridge and
//! the service adapter still build: they need no hyper feature. The facade
//! crate `moonpool` exposes this crate as `moonpool::hyper` behind its
//! `hyper` feature.
//!
//! ## Determinism
//!
//! Everything hyper does with time and task spawning goes through the
//! providers: h2 keepalive pings and timeouts read the provider clock, and
//! hyper's internal futures are provider tasks. Inside a simulation, the
//! connection lifecycle is then reproducible from a seed, chaos included.
//!
//! Two clock reads sit below hyper's `Timer`. This crate neutralises them
//! instead of routing them:
//!
//! - The server's automatic `Date` header is disabled ([`H2Server`]), because
//!   it reads the system clock and changes the HPACK bytes between runs.
//! - h2 expires locally reset streams on `std::time::Instant::now()` after
//!   one real second by default. The client sets `reset_stream_duration` to
//!   `Duration::MAX`, so the count cap alone bounds them
//!   ([`ReconnectingChannel`]). hyper has no such knob on the server side, so
//!   a server that resets streams keeps a residual dependence on wall time.
//!
//! ## Examples
//!
//! `moonpool-sim-examples` runs a tonic gRPC service (`tonic_grpc`) and an
//! axum web service (`axum_web`) on these adapters under simulation, with
//! network chaos and process reboots.
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

#[cfg(feature = "client")]
mod client;
mod config;
#[cfg(feature = "client")]
mod error;
mod io;
mod rt;
#[cfg(feature = "server")]
mod server;
mod service;

#[cfg(feature = "client")]
pub use client::{ChannelConfig, H2Channel, ReconnectingChannel, ResponseFuture};
pub use config::KeepAlive;
#[cfg(feature = "client")]
pub use error::ChannelError;
pub use io::HyperIo;
pub use rt::{HyperExecutor, HyperTimer};
#[cfg(feature = "server")]
pub use server::{DEFAULT_DRAIN_TIMEOUT, H2Server, H2ServerConfig, ServeError};
pub use service::{TowerToHyperService, TowerToHyperServiceFuture};
