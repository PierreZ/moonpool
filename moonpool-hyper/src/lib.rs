//! # moonpool-hyper
//!
//! hyper 1.x integration for the moonpool provider traits.
//!
//! hyper is runtime-agnostic by design, but every adapter it ships with
//! (`hyper_util`'s `TokioExecutor`, `TokioTimer`, `TokioIo`) is tokio-specific.
//! This crate supplies the same hooks over moonpool's provider traits, so an
//! HTTP/2 stack (tonic gRPC, axum, plain hyper) runs unchanged on the tokio
//! providers in production and on the deterministic simulation providers under
//! test.
//!
//! ## What it provides
//!
//! | Type | hyper hook | Provider |
//! |------|------------|----------|
//! | [`HyperExecutor`] | [`hyper::rt::Executor`] | [`TaskProvider`](moonpool_core::TaskProvider) |
//! | [`HyperTimer`] | [`hyper::rt::Timer`] | [`TimeProvider`](moonpool_core::TimeProvider) |
//! | [`HyperIo`] | [`hyper::rt::Read`] + [`hyper::rt::Write`] | any [`NetworkProvider`](moonpool_core::NetworkProvider) stream |
//! | [`TowerToHyperService`] | [`hyper::service::Service`] | (adapts a tower service) |
//!
//! [`KeepAlive`] carries the h2 PING settings that both sides accept.
//!
//! ## Determinism
//!
//! Everything hyper does with time and task spawning goes through the
//! providers: h2 keepalive pings and timeouts read the provider clock, and
//! hyper's internal futures are provider tasks. Inside a simulation that makes
//! connection lifecycles reproducible from a seed, chaos included.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

#[cfg(feature = "client")]
mod client;
mod config;
#[cfg(feature = "client")]
mod error;
mod io;
mod rt;
mod service;

#[cfg(feature = "client")]
pub use client::{ChannelConfig, H2Channel, ReconnectingChannel, ResponseFuture};
pub use config::KeepAlive;
#[cfg(feature = "client")]
pub use error::ChannelError;
pub use io::HyperIo;
pub use rt::{HyperExecutor, HyperTimer};
pub use service::{TowerToHyperService, TowerToHyperServiceFuture};
