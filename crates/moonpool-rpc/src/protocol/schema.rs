//! Stable method and schema identity.

use crate::codec::Wire;

/// Stable identifier of one method of a service.
///
/// Chosen by the application and written down as a constant; never derived
/// from a Rust type or function name, a declaration order or a layout, so
/// renaming or reordering Rust items cannot change wire meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodId(u64);

impl MethodId {
    /// Wrap an application-chosen identifier.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for MethodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "method:{:#018x}", self.0)
    }
}

/// Stable identifier of one version of a method's request/reply contract.
///
/// Also application-chosen. Bump it when a change is not compatible under
/// the codec's evolution rules (see [`codec`](crate::codec)): an endpoint
/// then rejects callers of the old contract with
/// [`RpcError::SchemaMismatch`](crate::RpcError::SchemaMismatch) instead of
/// misreading their bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaId(u64);

impl SchemaId {
    /// Wrap an application-chosen identifier.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SchemaId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "schema:{:#018x}", self.0)
    }
}

/// One typed request/reply method.
///
/// Implement it on a marker type per method. The transport admits a request
/// only when its method, schema and codec identifiers all equal the ones the
/// endpoint registered, checked in that order **before** any byte of the
/// body is decoded.
///
/// ```
/// # #[cfg(feature = "prost")] {
/// use moonpool_rpc::{MethodId, RpcMethod, SchemaId};
///
/// #[derive(Clone, PartialEq, prost::Message)]
/// pub struct EchoRequest {
///     #[prost(string, tag = "1")]
///     pub text: String,
/// }
///
/// /// Echo a string back.
/// pub struct Echo;
///
/// impl RpcMethod for Echo {
///     type Request = EchoRequest;
///     type Reply = String;
///     const METHOD: MethodId = MethodId::new(0x6563_686f);
///     const SCHEMA: SchemaId = SchemaId::new(1);
///     const NAME: &'static str = "echo";
/// }
/// # }
/// ```
pub trait RpcMethod: Send + Sync + 'static {
    /// The request body.
    type Request: Wire;
    /// The reply body.
    type Reply: Wire;
    /// The stable identity of the method.
    const METHOD: MethodId;
    /// The stable identity of this version of its contract.
    const SCHEMA: SchemaId;
    /// A human-readable name for traces and errors. Never sent on the wire.
    const NAME: &'static str;
}
