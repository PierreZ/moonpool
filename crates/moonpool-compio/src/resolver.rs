//! Hostname resolution on the compio runtime: [`CompioResolver`].

use std::future::Future;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs as _};

use moonpool_core::{Resolver, split_host_port};

/// Operating-system resolver: `getaddrinfo` (through
/// [`std::net::ToSocketAddrs`]) on compio's blocking pool, so a slow lookup
/// never stalls the runtime thread.
///
/// A numeric target resolves to itself without a lookup. A name with no
/// address is [`io::ErrorKind::NotFound`], and a target that is not
/// `host:port` is [`io::ErrorKind::InvalidInput`]. Resolving needs a running
/// compio runtime.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioResolver;

impl CompioResolver {
    /// Create an operating-system resolver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[tracing::instrument(skip_all, fields(target = %target))]
async fn resolve(target: String) -> io::Result<Vec<SocketAddr>> {
    let (host, port) = split_host_port(&target)?;
    let host = host.to_owned();
    let addresses: Vec<SocketAddr> =
        compio::runtime::spawn_blocking(move || (host.as_str(), port).to_socket_addrs())
            .await
            .map_err(|error| io::Error::other(format!("blocking lookup failed: {error}")))??
            .collect();
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{target} resolved to no address"),
        ));
    }
    Ok(addresses)
}

impl Resolver for CompioResolver {
    fn resolve(&self, target: &str) -> impl Future<Output = io::Result<Vec<SocketAddr>>> + Send {
        // Only `Send` state is held across the await (the blocking pool's
        // handle is `Send`), so no thread-affinity wrapper is needed.
        resolve(target.to_owned())
    }
}
