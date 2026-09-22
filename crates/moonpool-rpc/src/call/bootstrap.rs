//! Well-known bootstrap endpoints: hostname resolution with a cache that a
//! failed connection invalidates, and an explicit retry helper.
//!
//! This is the one place hostnames and retries live. A dynamic
//! [`ServiceRef`] is never resolved and never refreshed: its incarnation is
//! part of its identity, and learning a new one is application work. A
//! [`WellKnownRef`] names a service by address and [`WellKnownId`] alone,
//! so it reaches whichever incarnation currently registered that id — which
//! is exactly what bootstrap needs and nothing else should use.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_core::{Providers, RandomProvider, Resolver, TimeProvider};

use super::client::ServiceRef;
use crate::endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation, WellKnownId};
use crate::error::{ErrorReason, Execution, RpcError};
use crate::protocol::RpcMethod;
use crate::transport::RpcHandle;

/// Where a well-known endpoint lives: a resolved address, or a `host:port`
/// to resolve.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BootstrapAddress {
    /// Already resolved.
    Resolved(SocketAddr),
    /// A `host:port` resolved through the client's
    /// [`Resolver`](moonpool_core::Resolver).
    Host(String),
}

impl BootstrapAddress {
    /// A numeric `ip:port` becomes [`Resolved`](Self::Resolved), anything
    /// else [`Host`](Self::Host).
    #[must_use]
    pub fn parse(target: &str) -> Self {
        target
            .parse::<SocketAddr>()
            .map_or_else(|_| Self::Host(target.to_string()), Self::Resolved)
    }
}

impl std::fmt::Display for BootstrapAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolved(address) => write!(f, "{address}"),
            Self::Host(host) => f.write_str(host),
        }
    }
}

/// A reference to the well-known endpoint `id` serving `M` at `address`.
///
/// Plain data. Unlike a dynamic [`ServiceRef`] it names no incarnation: a
/// restarted server that registers the same id answers it again.
pub struct WellKnownRef<M: RpcMethod> {
    address: BootstrapAddress,
    id: WellKnownId,
    access: AccessClass,
    _method: PhantomData<fn() -> M>,
}

impl<M: RpcMethod> WellKnownRef<M> {
    /// The endpoint `id` at `address`.
    #[must_use]
    pub fn new(address: BootstrapAddress, id: WellKnownId, access: AccessClass) -> Self {
        Self {
            address,
            id,
            access,
            _method: PhantomData,
        }
    }

    /// Where the endpoint lives.
    #[must_use]
    pub fn address(&self) -> &BootstrapAddress {
        &self.address
    }

    /// The well-known id.
    #[must_use]
    pub fn id(&self) -> WellKnownId {
        self.id
    }

    /// The endpoint at one resolved address, as a callable reference.
    #[must_use]
    pub fn at(&self, address: SocketAddr) -> ServiceRef<M> {
        ServiceRef::new(
            Endpoint::new(
                address,
                // Admission ignores the incarnation of a well-known token.
                Incarnation::from_raw(0),
                EndpointToken::well_known(self.id),
            ),
            self.access,
        )
    }
}

impl<M: RpcMethod> Clone for WellKnownRef<M> {
    fn clone(&self) -> Self {
        Self::new(self.address.clone(), self.id, self.access)
    }
}

impl<M: RpcMethod> std::fmt::Debug for WellKnownRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WellKnownRef")
            .field("method", &M::NAME)
            .field("address", &self.address)
            .field("id", &self.id)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// When [`BootstrapClient::retry_get_reply`] tries again.
///
/// Plain data. Retrying is the application's decision: an attempt that may
/// have executed is retried only with `retry_ambiguous`, because the retry
/// can execute the request twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Pause after the first failed attempt; doubles each time.
    pub initial_backoff: Duration,
    /// Longest pause between attempts.
    pub max_backoff: Duration,
    /// Deadline of each attempt.
    pub attempt_timeout: Duration,
    /// Give up after this many attempts (`None`: keep trying).
    pub max_attempts: Option<u32>,
    /// Also retry after [`Execution::MaybeExecuted`] outcomes (disconnects,
    /// timeouts after sending, broken promises).
    pub retry_ambiguous: bool,
}

impl Default for RetryPolicy {
    /// `FoundationDB`'s `HOSTNAME_RECONNECT_INIT_INTERVAL` and
    /// `HOSTNAME_RECONNECT_MAX_INTERVAL`; unambiguous failures only.
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(1),
            attempt_timeout: Duration::from_secs(5),
            max_attempts: None,
            retry_ambiguous: false,
        }
    }
}

impl RetryPolicy {
    /// Whether `error` may be retried under this policy.
    ///
    /// Contract errors (method, schema or codec mismatch, frame limits,
    /// encoding, reply problems) and shutdown never are; anything proven
    /// [`Execution::NotAdmitted`] is (a lookup or connect failure, an
    /// endpoint not registered yet, overload, a timeout before sending);
    /// [`Execution::MaybeExecuted`] only with `retry_ambiguous`;
    /// [`Execution::Executed`] never.
    #[must_use]
    pub fn permits(&self, error: &RpcError) -> bool {
        let contract = matches!(
            error.reason(),
            ErrorReason::MethodMismatch { .. }
                | ErrorReason::SchemaMismatch { .. }
                | ErrorReason::CodecMismatch { .. }
                | ErrorReason::FrameTooLarge { .. }
                | ErrorReason::Encode(_)
                | ErrorReason::MalformedRequest
                | ErrorReason::ReplyTooLarge
                | ErrorReason::ReplyEncodeFailed
                | ErrorReason::MalformedReply(_)
                | ErrorReason::Shutdown
                | ErrorReason::NotListening
                | ErrorReason::AlreadyRegistered
        );
        !contract
            && match error.execution() {
                Execution::NotAdmitted => true,
                Execution::MaybeExecuted => self.retry_ambiguous,
                _ => false,
            }
    }
}

/// Counters of one [`BootstrapClient`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapStats {
    /// Resolver lookups performed (cache misses).
    pub lookups: u64,
    /// Lookups that failed.
    pub lookup_failures: u64,
    /// Answers served from the cache.
    pub cache_hits: u64,
    /// Cached answers dropped after a connection failure.
    pub invalidations: u64,
    /// Attempts [`BootstrapClient::retry_get_reply`] made after the first.
    pub retries: u64,
}

#[derive(Default)]
struct Counters {
    lookups: AtomicU64,
    lookup_failures: AtomicU64,
    cache_hits: AtomicU64,
    invalidations: AtomicU64,
    retries: AtomicU64,
}

struct Cached {
    addresses: Vec<SocketAddr>,
    expires: Duration,
}

/// Calls well-known endpoints through a runtime, resolving hostnames with
/// `R` and caching answers for `ttl` of provider time.
///
/// A cached answer is dropped early when a call through it fails to
/// connect or loses its connection, so the next attempt resolves again
/// (`FoundationDB`'s `removeCachedDNS`). A lookup failure
/// ([`ErrorReason::LookupFailed`]), a connection failure
/// ([`ErrorReason::ConnectFailed`], [`ErrorReason::Disconnected`]) and an
/// endpoint failure ([`ErrorReason::EndpointNotFound`]) stay distinct.
pub struct BootstrapClient<P: Providers, R: Resolver> {
    rpc: RpcHandle<P>,
    resolver: R,
    ttl: Duration,
    cache: Arc<Mutex<BTreeMap<String, Cached>>>,
    counters: Arc<Counters>,
}

impl<P: Providers, R: Resolver> Clone for BootstrapClient<P, R> {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            resolver: self.resolver.clone(),
            ttl: self.ttl,
            cache: Arc::clone(&self.cache),
            counters: Arc::clone(&self.counters),
        }
    }
}

impl<P: Providers, R: Resolver> std::fmt::Debug for BootstrapClient<P, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapClient")
            .field("ttl", &self.ttl)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

fn shutdown() -> RpcError {
    RpcError::not_admitted(ErrorReason::Shutdown)
}

impl<P: Providers, R: Resolver> BootstrapClient<P, R> {
    /// A client calling through `rpc`, resolving with `resolver` and
    /// trusting an answer for `ttl`.
    #[must_use]
    pub fn new(rpc: &RpcHandle<P>, resolver: R, ttl: Duration) -> Self {
        Self {
            rpc: rpc.clone(),
            resolver,
            ttl,
            cache: Arc::new(Mutex::new(BTreeMap::new())),
            counters: Arc::new(Counters::default()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Cached>> {
        self.cache
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> BootstrapStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        BootstrapStats {
            lookups: load(&self.counters.lookups),
            lookup_failures: load(&self.counters.lookup_failures),
            cache_hits: load(&self.counters.cache_hits),
            invalidations: load(&self.counters.invalidations),
            retries: load(&self.counters.retries),
        }
    }

    /// Drop the cached answer for `host`, if any.
    pub fn invalidate(&self, host: &str) {
        if self.cache().remove(host).is_some() {
            self.counters.invalidations.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One address for `address`: itself if resolved, else a cached or
    /// fresh answer, one of its addresses picked at random.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::LookupFailed`] ([`Execution::NotAdmitted`]), or
    /// [`ErrorReason::Shutdown`].
    pub async fn resolve(&self, address: &BootstrapAddress) -> Result<SocketAddr, RpcError> {
        let host = match address {
            BootstrapAddress::Resolved(resolved) => return Ok(*resolved),
            BootstrapAddress::Host(host) => host,
        };
        let (time, random) = {
            let shared = self.rpc.upgrade().ok_or_else(shutdown)?;
            (shared.time().clone(), shared.random().clone())
        };
        let now = time.now();
        let cached = self
            .cache()
            .get(host)
            .filter(|entry| entry.expires > now)
            .map(|entry| entry.addresses.clone());
        let addresses = if let Some(addresses) = cached {
            self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
            addresses
        } else {
            self.counters.lookups.fetch_add(1, Ordering::Relaxed);
            let answer = self.resolver.resolve(host).await.map_err(|error| {
                self.counters
                    .lookup_failures
                    .fetch_add(1, Ordering::Relaxed);
                RpcError::not_admitted(ErrorReason::LookupFailed(format!("{host}: {error}")))
            })?;
            self.cache().insert(
                host.clone(),
                Cached {
                    addresses: answer.clone(),
                    expires: time.now().saturating_add(self.ttl),
                },
            );
            answer
        };
        match addresses.len() {
            0 => Err(RpcError::not_admitted(ErrorReason::LookupFailed(format!(
                "{host}: no address"
            )))),
            1 => Ok(addresses[0]),
            count => Ok(addresses[random.random_range(0..count)]),
        }
    }

    /// One at-most-once attempt at the well-known endpoint, with a deadline.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::LookupFailed`] when the name does not resolve, else
    /// the attempt's own [`RpcError`].
    pub async fn try_get_reply<M: RpcMethod>(
        &self,
        target: &WellKnownRef<M>,
        request: &M::Request,
        timeout: Duration,
    ) -> Result<M::Reply, RpcError> {
        let address = self.resolve(target.address()).await?;
        let outcome = target
            .at(address)
            .bind(&self.rpc)
            .try_get_reply_within(request, timeout)
            .await;
        if let (Err(error), BootstrapAddress::Host(host)) = (&outcome, target.address())
            && matches!(
                error.reason(),
                ErrorReason::ConnectFailed(_) | ErrorReason::Disconnected
            )
        {
            // The address may have moved: resolve again next time.
            self.invalidate(host);
        }
        outcome
    }

    /// Call the well-known endpoint until it answers or `policy` says stop
    /// (`FoundationDB`'s `retryGetReplyFromHostname`).
    ///
    /// Each attempt is at most once; the helper decides whether to try
    /// again ([`RetryPolicy::permits`]) after a jittered, doubling pause.
    /// Every retry may re-resolve the name, never a dynamic reference: it
    /// cannot substitute a different incarnation of a dynamic service.
    ///
    /// # Errors
    ///
    /// The last attempt's [`RpcError`] once the policy stops retrying; its
    /// execution knowledge is weakened to [`Execution::MaybeExecuted`] if
    /// any earlier attempt may have executed.
    pub async fn retry_get_reply<M: RpcMethod>(
        &self,
        target: &WellKnownRef<M>,
        request: &M::Request,
        policy: &RetryPolicy,
    ) -> Result<M::Reply, RpcError> {
        let mut backoff = policy.initial_backoff;
        let mut attempts = 0u32;
        let mut ambiguous = false;
        loop {
            attempts = attempts.saturating_add(1);
            let error = match self
                .try_get_reply(target, request, policy.attempt_timeout)
                .await
            {
                Ok(reply) => return Ok(reply),
                Err(error) => error,
            };
            let exhausted = policy.max_attempts.is_some_and(|max| attempts >= max);
            if exhausted || !policy.permits(&error) {
                return Err(if ambiguous {
                    error.at_least(Execution::MaybeExecuted)
                } else {
                    error
                });
            }
            ambiguous |= error.execution() != Execution::NotAdmitted;
            self.counters.retries.fetch_add(1, Ordering::Relaxed);
            let (time, draw) = {
                let shared = self.rpc.upgrade().ok_or_else(shutdown)?;
                (shared.time().clone(), shared.random().random_ratio())
            };
            // A random share of the backoff keeps retrying clients apart.
            let _ = time.sleep(backoff.mul_f64(0.5 + draw / 2.0)).await;
            backoff = (backoff * 2).min(policy.max_backoff.max(policy.initial_backoff));
        }
    }
}
