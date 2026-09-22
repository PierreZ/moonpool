//! Serialisable service references and explicitly bound clients.

use std::marker::PhantomData;
use std::sync::Weak;

use futures::channel::oneshot;
use moonpool_core::Providers;

use crate::codec::{CodecId, DecodeError, EncodeError, Wire, encode_to_vec};
use crate::endpoint::{Endpoint, EndpointToken, Incarnation};
use crate::error::{CallIdentity, RpcError};
use crate::protocol::{MethodId, Reader, RpcMethod, SchemaId, Writer};
use crate::transport::RpcHandle;

/// Version byte of the [`ServiceRef`] byte layout.
const SERVICE_REF_VERSION: u8 = 1;

/// A typed reference to a dynamic endpoint serving method `M`.
///
/// Plain routing data: it keeps nothing alive and needs no runtime to
/// exist, be stored or be decoded. Its byte form ([`to_bytes`](Self::to_bytes),
/// also its [`Wire`] encoding under [`CodecId::RPC`], so a reference can be
/// a request or reply body) is a fixed layout owned by this crate:
///
/// ```text
/// version u8 = 1 | method u64 | schema u64 | request codec u16 | reply codec u16
///   | incarnation u64 | token.index u32 | token.generation u32
///   | address length u16 | address (UTF-8)
/// ```
///
/// Decoding checks the method, schema and both codecs against `M`, so a
/// reference cannot be decoded as the wrong method type. Bind it to a runtime
/// with [`bind`](Self::bind) to call it.
pub struct ServiceRef<M: RpcMethod> {
    endpoint: Endpoint,
    _method: PhantomData<fn() -> M>,
}

impl<M: RpcMethod> ServiceRef<M> {
    /// Claim that `endpoint` serves `M`.
    ///
    /// Nothing is checked locally: the server validates the incarnation,
    /// token, method, schema and codec of every request before any byte
    /// reaches a handler.
    #[must_use]
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            _method: PhantomData,
        }
    }

    /// The addressed endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
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
        let address = self.endpoint.address().as_bytes();
        let mut out = Writer::new();
        out.u8(SERVICE_REF_VERSION)
            .u64(M::METHOD.get())
            .u64(M::SCHEMA.get())
            .u16(<M::Request as Wire>::CODEC.get())
            .u16(<M::Reply as Wire>::CODEC.get())
            .u64(self.endpoint.incarnation().get())
            .u32(self.endpoint.token().index())
            .u32(self.endpoint.token().generation())
            // Addresses are `ip:port` strings; longer ones are truncated to
            // the field's range and will simply not connect.
            .u16(u16::try_from(address.len()).unwrap_or(u16::MAX))
            .bytes(&address[..address.len().min(usize::from(u16::MAX))]);
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
        let method = MethodId::new(input.u64().ok_or_else(truncated)?);
        let schema = SchemaId::new(input.u64().ok_or_else(truncated)?);
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
        let incarnation = Incarnation::from_raw(input.u64().ok_or_else(truncated)?);
        let index = input.u32().ok_or_else(truncated)?;
        let generation = input.u32().ok_or_else(truncated)?;
        let address_len = input.u16().ok_or_else(truncated)?;
        let address = input
            .slice(usize::from(address_len))
            .ok_or_else(truncated)?;
        let address = std::str::from_utf8(address)
            .map_err(|_| DecodeError("service reference address is not UTF-8".into()))?;
        if !input.is_empty() {
            return Err(DecodeError("trailing bytes after service reference".into()));
        }
        Ok(Self::new(Endpoint::new(
            address,
            incarnation,
            EndpointToken::from_parts(index, generation),
        )))
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
        Self::new(self.endpoint.clone())
    }
}

impl<M: RpcMethod> PartialEq for ServiceRef<M> {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint
    }
}

impl<M: RpcMethod> Eq for ServiceRef<M> {}

impl<M: RpcMethod> std::fmt::Debug for ServiceRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRef")
            .field("method", &M::NAME)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// A [`ServiceRef`] bound to a runtime: the calling side.
///
/// Cloning is cheap. A client holds the runtime weakly: it never keeps a
/// dropped runtime serving, and calls on it then fail with
/// [`RpcError::Shutdown`].
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

impl<P: Providers, M: RpcMethod> ServiceClient<P, M> {
    /// The reference this client calls.
    #[must_use]
    pub fn target(&self) -> &ServiceRef<M> {
        &self.target
    }

    /// Send one request attempt and wait for its single completion.
    ///
    /// The request executes on the server zero or one times: nothing is
    /// retransmitted, on this connection or any later one. The outcome is
    /// the reply, a rejection that proves non-execution, or an ambiguous
    /// failure; see [`RpcError::execution`].
    ///
    /// Cancelling the returned future (dropping it, or a timeout around it)
    /// releases the reply route at once; a reply arriving later is counted
    /// and discarded. Cancellation does not retract a request already sent:
    /// the server may still execute it.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`]; the error states what it proves about execution.
    pub async fn try_get_reply(&self, request: &M::Request) -> Result<M::Reply, RpcError> {
        let body = encode_to_vec(request).map_err(|e| RpcError::Encode(e.0))?;
        let identity = CallIdentity {
            method: M::METHOD,
            schema: M::SCHEMA,
            codec: <M::Request as Wire>::CODEC,
        };
        let (receiver, guard) = {
            // Hold the runtime only while starting the call: the wait below
            // must not keep a dropped driver's state alive.
            let shared = self.rpc.upgrade().ok_or(RpcError::Shutdown)?;
            shared.start_call(&self.target.endpoint, identity, body)?
        };
        let outcome = receiver.await;
        guard.disarm();
        let (codec, bytes) = outcome.map_err(|oneshot::Canceled| RpcError::Shutdown)??;
        let expected = <M::Reply as Wire>::CODEC;
        if codec != expected {
            return Err(RpcError::CodecMismatch {
                sent: codec,
                expected,
            });
        }
        <M::Reply as Wire>::decode(&bytes).map_err(|e| RpcError::MalformedReply(e.0))
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
    fn abandon(&self, call_id: u64);
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
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(owner) = self.owner.upgrade()
        {
            owner.abandon(self.call_id);
        }
    }
}
