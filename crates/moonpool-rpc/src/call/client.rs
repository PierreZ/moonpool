//! Serialisable service references and explicitly bound clients.

use std::marker::PhantomData;
use std::sync::Weak;
use std::time::Duration;

use futures::channel::oneshot;
use moonpool_core::{Providers, TimeProvider};

use crate::codec::{CodecId, DecodeError, EncodeError, Wire, encode_to_vec};
use crate::endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation};
use crate::error::{CallIdentity, ErrorReason, Execution, RpcError};
use crate::protocol::{MethodId, Reader, RpcMethod, SchemaVersion, Writer};
use crate::transport::{ReplyBytes, RpcHandle};

/// Version byte of the [`ServiceRef`] byte layout.
const SERVICE_REF_VERSION: u8 = 1;

/// A typed reference to a dynamic endpoint serving method `M`.
///
/// Plain routing data: it keeps nothing alive and needs no runtime to
/// exist, be stored or be decoded. Its byte form ([`to_bytes`](Self::to_bytes),
/// also its [`Wire`] encoding under [`CodecId::RPC`], so a reference can be
/// a request or reply body) is a fixed little-endian layout owned by this
/// crate:
///
/// ```text
/// version u8 = 1 | method u32 | schema u16 | request codec u16 | reply codec u16
///   | access u8 | incarnation u128 | token.index u64 | token.generation u32
///   | address: family u8 (4 or 6) | ip (4 or 16 bytes) | port u16
/// ```
///
/// Decoding checks the method, schema and both codecs against `M`, so a
/// reference cannot be decoded as the wrong method type. Bind it to a runtime
/// with [`bind`](Self::bind) to call it.
pub struct ServiceRef<M: RpcMethod> {
    endpoint: Endpoint,
    access: AccessClass,
    _method: PhantomData<fn() -> M>,
}

impl<M: RpcMethod> ServiceRef<M> {
    /// Claim that `endpoint` serves `M` with the given access class.
    ///
    /// Nothing is checked locally: the server validates the incarnation,
    /// token, method, schema and codec of every request before any byte
    /// reaches a handler.
    #[must_use]
    pub fn new(endpoint: Endpoint, access: AccessClass) -> Self {
        Self {
            endpoint,
            access,
            _method: PhantomData,
        }
    }

    /// The addressed endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The access class the endpoint was registered with.
    #[must_use]
    pub fn access(&self) -> AccessClass {
        self.access
    }

    /// Bind to a runtime, producing a client that calls through it.
    #[must_use]
    pub fn bind<P: Providers>(&self, rpc: &RpcHandle<P>) -> ServiceClient<P, M> {
        ServiceClient {
            rpc: rpc.clone(),
            target: self.clone(),
        }
    }

    /// The reference's byte form (see the type docs for the layout).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.u8(SERVICE_REF_VERSION)
            .u32(M::METHOD.get())
            .u16(M::SCHEMA.get())
            .u16(<M::Request as Wire>::CODEC.get())
            .u16(<M::Reply as Wire>::CODEC.get())
            .u8(self.access.to_byte())
            .u128(self.endpoint.incarnation().get())
            .u64(self.endpoint.token().index())
            .u32(self.endpoint.token().generation())
            .socket_addr(self.endpoint.address());
        out.0
    }

    /// Decode a reference, checking that it serves `M`.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] for a malformed layout, an unknown version, or a
    /// method, schema or codec that is not `M`'s.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let truncated = || DecodeError("truncated service reference".into());
        let mut input = Reader::new(bytes);
        let version = input.u8().ok_or_else(truncated)?;
        if version != SERVICE_REF_VERSION {
            return Err(DecodeError(format!(
                "unknown service reference version {version}"
            )));
        }
        let method = MethodId::new(input.u32().ok_or_else(truncated)?);
        let schema = SchemaVersion::new(input.u16().ok_or_else(truncated)?);
        let request_codec = CodecId::new(input.u16().ok_or_else(truncated)?);
        let reply_codec = CodecId::new(input.u16().ok_or_else(truncated)?);
        if method != M::METHOD
            || schema != M::SCHEMA
            || request_codec != <M::Request as Wire>::CODEC
            || reply_codec != <M::Reply as Wire>::CODEC
        {
            return Err(DecodeError(format!(
                "service reference serves {method}/{schema} ({request_codec}->{reply_codec}), \
                 not {} ({}/{})",
                M::NAME,
                M::METHOD,
                M::SCHEMA
            )));
        }
        let access = AccessClass::from_byte(input.u8().ok_or_else(truncated)?)
            .ok_or_else(|| DecodeError("unknown access class".into()))?;
        let incarnation = Incarnation::from_raw(input.u128().ok_or_else(truncated)?);
        let index = input.u64().ok_or_else(truncated)?;
        let generation = input.u32().ok_or_else(truncated)?;
        let address = input
            .socket_addr()
            .ok_or_else(|| DecodeError("invalid service reference address".into()))?;
        if !input.is_empty() {
            return Err(DecodeError("trailing bytes after service reference".into()));
        }
        Ok(Self::new(
            Endpoint::new(
                address,
                incarnation,
                EndpointToken::from_parts(index, generation),
            ),
            access,
        ))
    }
}

impl<M: RpcMethod> Wire for ServiceRef<M> {
    const CODEC: CodecId = CodecId::RPC;

    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
        buf.extend_from_slice(&self.to_bytes());
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Self::from_bytes(bytes)
    }
}

impl<M: RpcMethod> Clone for ServiceRef<M> {
    fn clone(&self) -> Self {
        Self::new(self.endpoint, self.access)
    }
}

impl<M: RpcMethod> PartialEq for ServiceRef<M> {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint && self.access == other.access
    }
}

impl<M: RpcMethod> Eq for ServiceRef<M> {}

impl<M: RpcMethod> std::fmt::Debug for ServiceRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRef")
            .field("method", &M::NAME)
            .field("endpoint", &self.endpoint)
            .field("access", &self.access)
            .finish()
    }
}

/// A [`ServiceRef`] bound to a runtime: the calling side.
///
/// Cloning is cheap. A client holds the runtime weakly: it never keeps a
/// dropped runtime serving, and calls on it then fail with
/// [`ErrorReason::Shutdown`].
pub struct ServiceClient<P: Providers, M: RpcMethod> {
    rpc: RpcHandle<P>,
    target: ServiceRef<M>,
}

impl<P: Providers, M: RpcMethod> Clone for ServiceClient<P, M> {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            target: self.target.clone(),
        }
    }
}

impl<P: Providers, M: RpcMethod> std::fmt::Debug for ServiceClient<P, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceClient")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A started call: its completion, the guard that releases its route, and
/// the runtime's clock.
type StartedCall<T> = (
    oneshot::Receiver<Result<ReplyBytes, RpcError>>,
    CallGuard,
    T,
);

impl<P: Providers, M: RpcMethod> ServiceClient<P, M> {
    /// The reference this client calls.
    #[must_use]
    pub fn target(&self) -> &ServiceRef<M> {
        &self.target
    }

    fn start(&self, request: &M::Request) -> Result<StartedCall<P::Time>, RpcError> {
        let body = encode_to_vec(request)
            .map_err(|error| RpcError::not_admitted(ErrorReason::Encode(error.0)))?;
        let identity = CallIdentity {
            method: M::METHOD,
            schema: M::SCHEMA,
            codec: <M::Request as Wire>::CODEC,
        };
        // Hold the runtime only while starting the call: the wait must not
        // keep a dropped driver's state alive.
        let shared = self
            .rpc
            .upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?;
        let (receiver, guard) = shared.start_call(self.target.endpoint(), identity, body)?;
        Ok((receiver, guard, shared.time().clone()))
    }

    fn finish(
        outcome: Result<Result<ReplyBytes, RpcError>, oneshot::Canceled>,
    ) -> Result<M::Reply, RpcError> {
        let (codec, bytes) = outcome.map_err(|oneshot::Canceled| {
            // The runtime was dropped with the call pending.
            RpcError::new(ErrorReason::Shutdown, Execution::MaybeExecuted)
        })??;
        let expected = <M::Reply as Wire>::CODEC;
        if codec != expected {
            return Err(RpcError::new(
                ErrorReason::CodecMismatch {
                    sent: codec,
                    expected,
                },
                Execution::Executed,
            ));
        }
        <M::Reply as Wire>::decode(&bytes).map_err(|error| {
            RpcError::new(ErrorReason::MalformedReply(error.0), Execution::Executed)
        })
    }

    /// Send one request attempt and wait for its single completion.
    ///
    /// The request executes on the server zero or one times: nothing is
    /// retransmitted, on this connection or any later one. The outcome is
    /// the reply, or an [`RpcError`] stating what it proves about execution.
    ///
    /// Cancelling the returned future (dropping it, or a timeout around it)
    /// releases the reply route at once; a reply arriving later is counted
    /// and discarded. Cancellation does not retract a request already sent:
    /// the server may still execute it. Prefer
    /// [`try_get_reply_within`](Self::try_get_reply_within) for a deadline
    /// that reports what it knows.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`].
    pub async fn try_get_reply(&self, request: &M::Request) -> Result<M::Reply, RpcError> {
        let (receiver, guard, _) = self.start(request)?;
        let outcome = receiver.await;
        guard.disarm();
        Self::finish(outcome)
    }

    /// [`try_get_reply`](Self::try_get_reply) with a deadline on provider
    /// time.
    ///
    /// When the deadline passes first the call is abandoned and fails with
    /// [`ErrorReason::Timeout`]: [`Execution::NotAdmitted`] if the request
    /// had not begun to leave this process, [`Execution::MaybeExecuted`]
    /// otherwise.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`], including the timeout.
    pub async fn try_get_reply_within(
        &self,
        request: &M::Request,
        timeout: Duration,
    ) -> Result<M::Reply, RpcError> {
        let (mut receiver, guard, time) = self.start(request)?;
        let sleep = time.sleep(timeout);
        futures::pin_mut!(sleep);
        match futures::future::select(&mut receiver, sleep).await {
            futures::future::Either::Left((outcome, _)) => {
                guard.disarm();
                Self::finish(outcome)
            }
            futures::future::Either::Right(_) => match guard.expire() {
                Some(transmitted) => Err(RpcError::new(
                    ErrorReason::Timeout,
                    if transmitted {
                        Execution::MaybeExecuted
                    } else {
                        Execution::NotAdmitted
                    },
                )),
                // Completed in the same instant, or the runtime is gone: the
                // receiver holds the answer either way.
                None => Self::finish(receiver.await),
            },
        }
    }
}

/// Releases a pending call's reply route if the caller stops waiting.
pub(crate) struct CallGuard {
    owner: Weak<dyn CallOwner>,
    call_id: u64,
    armed: bool,
}

/// Forgets abandoned calls: implemented by the runtime's shared state.
pub(crate) trait CallOwner: Send + Sync {
    /// Forget a pending call; `Some(transmitted)` if it was still pending.
    fn abandon(&self, call_id: u64) -> Option<bool>;
}

impl CallGuard {
    pub(crate) fn new(owner: Weak<dyn CallOwner>, call_id: u64) -> Self {
        Self {
            owner,
            call_id,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }

    /// Abandon now and report whether the request had begun transmission;
    /// `None` when the call already completed or the runtime is gone.
    fn expire(mut self) -> Option<bool> {
        self.armed = false;
        self.owner.upgrade()?.abandon(self.call_id)
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(owner) = self.owner.upgrade()
        {
            let _ = owner.abandon(self.call_id);
        }
    }
}
