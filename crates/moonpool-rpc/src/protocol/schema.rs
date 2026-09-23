//! Stable method and schema identity.

use crate::codec::Wire;

/// Stable 32-bit identifier of one method of a service.
///
/// Chosen by the application and written down as a constant; never derived
/// from a Rust type or function name, a declaration order or a layout, so
/// renaming or reordering Rust items cannot change wire meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MethodId(u32);

impl MethodId {
    /// Wrap an application-chosen identifier.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for MethodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "method:{:#010x}", self.0)
    }
}

/// Version of one method's request/reply contract.
///
/// Also application-chosen. Bump it when a change is not compatible under
/// the codec's evolution rules (see [`codec`](crate::codec)): an endpoint
/// then rejects callers of the old contract with
/// [`ErrorReason::SchemaMismatch`](crate::ErrorReason::SchemaMismatch)
/// instead of misreading their bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaVersion(u16);

impl SchemaVersion {
    /// Wrap an application-chosen version.
    #[must_use]
    pub const fn new(version: u16) -> Self {
        Self(version)
    }

    /// The raw version.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "schema:v{}", self.0)
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
/// use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion};
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
///     const SCHEMA: SchemaVersion = SchemaVersion::new(1);
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
    /// The version of its contract.
    const SCHEMA: SchemaVersion;
    /// A human-readable name for traces and errors. Never sent on the wire.
    const NAME: &'static str;
}
