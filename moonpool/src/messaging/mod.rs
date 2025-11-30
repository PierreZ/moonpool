//! Messaging layer for moonpool.
//!
//! This module provides typed messaging abstractions on top of foundation's
//! raw transport primitives.
//!
//! # Modules
//!
//! - [`static`]: FDB-style static endpoints with fixed registration.
//!   Endpoints are registered at startup and remain for the lifetime of the transport.

pub mod r#static;
