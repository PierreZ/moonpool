//! Hostname resolution as an optional provider capability.
//!
//! [`NetworkProvider::connect`](crate::NetworkProvider::connect) accepts a
//! `host:port` string and resolves it internally, which hides everything a
//! client with its own address policy needs: the full list of addresses, a
//! lookup failure distinct from a connect failure, and a point where a cached
//! answer can be dropped and asked for again. [`Resolver`] exposes exactly
//! that one step.
//!
//! It is deliberately **not** part of the [`Providers`](crate::Providers)
//! bundle: most code never resolves names, and code that does chooses its own
//! resolver alongside its providers. Caching, time-to-live and invalidation
//! policy belong to the caller, not to the resolver.
//!
//! - [`TokioResolver`] (feature `tokio-net`) asks the operating system through
//!   `tokio::net::lookup_host`.
//! - `moonpool_sim::ScriptedResolver` answers from a table a simulation edits
//!   while it runs, so name changes, lookup failures and cache invalidation are
//!   deterministic.

use std::future::Future;
use std::io;
use std::net::SocketAddr;

/// Resolves a `host:port` target into socket addresses.
///
/// The target has the same shape as the string
/// [`NetworkProvider::connect`](crate::NetworkProvider::connect) takes. A
/// numeric target (`10.0.1.1:4500`, `[::1]:4500`) resolves to itself without a
/// lookup.
///
/// An `Ok` answer is never empty: an implementation reports a name with no
/// address as an error ([`io::ErrorKind::NotFound`]). The order of the
/// addresses is the resolver's; callers that pick one should do so with their
/// own (seeded) randomness.
pub trait Resolver: Clone + Send + Sync + 'static {
    /// Resolve `target` (`host:port`).
    fn resolve(&self, target: &str) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send;
}

/// Split `host:port` into its parts, accepting bracketed IPv6 hosts.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when the port is missing or not a number.
pub fn split_host_port(target: &str) -> io::Result<(&str, u16)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("resolver target {target:?} is not host:port"),
        )
    };
    let (host, port) = target.rsplit_once(':').ok_or_else(invalid)?;
    let port = port.parse::<u16>().map_err(|_| invalid())?;
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err(invalid());
    }
    Ok((host, port))
}

/// Production resolver: the operating system's lookup through
/// `tokio::net::lookup_host`.
#[cfg(feature = "tokio-net")]
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioResolver;

#[cfg(feature = "tokio-net")]
impl TokioResolver {
    /// Create an operating-system resolver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-net")]
impl Resolver for TokioResolver {
    async fn resolve(&self, target: &str) -> io::Result<Vec<SocketAddr>> {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host(target).await?.collect();
        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{target} resolved to no address"),
            ));
        }
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use super::split_host_port;

    #[test]
    fn splits_names_and_bracketed_ipv6() {
        assert_eq!(
            split_host_port("db.example:4500").ok(),
            Some(("db.example", 4500))
        );
        assert_eq!(split_host_port("[::1]:80").ok(), Some(("::1", 80)));
        assert!(split_host_port("no-port").is_err());
        assert!(split_host_port(":80").is_err());
        assert!(split_host_port("host:99999").is_err());
    }

    #[cfg(feature = "tokio-net")]
    #[tokio::test]
    async fn tokio_resolver_answers_literals_and_localhost() {
        use super::{Resolver, TokioResolver};
        let literal = TokioResolver::new().resolve("127.0.0.1:4500").await;
        assert_eq!(
            literal.ok(),
            Some(vec!["127.0.0.1:4500".parse().expect("literal address")])
        );
        let localhost = TokioResolver::new().resolve("localhost:4500").await;
        assert!(localhost.is_ok_and(|addresses| addresses.iter().all(|a| a.port() == 4500)));
        assert!(TokioResolver::new().resolve("missing-port").await.is_err());
    }
}
