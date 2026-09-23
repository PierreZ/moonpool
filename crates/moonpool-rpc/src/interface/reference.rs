//! Serialisable references: [`ServiceRef`] (one method) and
//! [`InterfaceRef`] (an endpoint group), both protobuf messages.

use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use moonpool_core::Providers;

use super::proto::{Counter, ProtoError, ProtoReader, ProtoWriter, Sink, Value};
use super::{InterfaceId, InterfaceMethod, RpcInterface};
use crate::call::client::ServiceClient;
use crate::codec::{DecodeError, Wire};
use crate::endpoint::{AccessClass, Endpoint, EndpointToken, Incarnation};
use crate::error::{ErrorReason, RpcError};
use crate::protocol::{RpcMethod, SchemaVersion};
use crate::transport::RpcHandle;

/// The protobuf field shapes routing data uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Varint,
    Fixed64,
    Bytes,
}

/// A field arrived with a wire type its tag never uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrongWireType;

/// The address octets as decoded: an IPv4 or IPv6 address, or whatever
/// length arrived (kept so re-encoding never repairs a defect).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum IpBytes {
    V4([u8; 4]),
    V6([u8; 16]),
    Invalid(Vec<u8>),
}

impl Default for IpBytes {
    fn default() -> Self {
        Self::Invalid(Vec::new())
    }
}

impl IpBytes {
    fn from_ip(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(ip) => Self::V4(ip.octets()),
            IpAddr::V6(ip) => Self::V6(ip.octets()),
        }
    }

    fn from_slice(bytes: &[u8]) -> Self {
        if let Ok(octets) = <[u8; 4]>::try_from(bytes) {
            Self::V4(octets)
        } else if let Ok(octets) = <[u8; 16]>::try_from(bytes) {
            Self::V6(octets)
        } else {
            Self::Invalid(bytes.to_vec())
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::V4(octets) => octets,
            Self::V6(octets) => octets,
            Self::Invalid(bytes) => bytes,
        }
    }

    fn ip(&self) -> Option<IpAddr> {
        match self {
            Self::V4(octets) => Some(IpAddr::from(*octets)),
            Self::V6(octets) => Some(IpAddr::from(*octets)),
            Self::Invalid(_) => None,
        }
    }
}

/// Field tags shared by both reference messages. Never renumber them.
mod tags {
    pub(super) const ACCESS: u32 = 5;
    pub(super) const INCARNATION_HIGH: u32 = 6;
    pub(super) const INCARNATION_LOW: u32 = 7;
    pub(super) const TOKEN_INDEX: u32 = 8;
    pub(super) const TOKEN_GENERATION: u32 = 9;
    pub(super) const IP: u32 = 10;
    pub(super) const PORT: u32 = 11;
}

/// The routing half of a reference, exactly as it was encoded or decoded.
///
/// Values are kept raw (`u64` where the wire has a varint) so that a
/// malformed reference stays malformed when re-encoded: validation happens
/// in [`Routing::check`], never by silently clamping.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Routing {
    access: u64,
    incarnation_high: u64,
    incarnation_low: u64,
    index: u64,
    generation: u64,
    ip: IpBytes,
    port: u64,
}

impl Routing {
    fn new(endpoint: &Endpoint, access: AccessClass) -> Self {
        let incarnation = endpoint.incarnation().get();
        let token = endpoint.token();
        Self {
            access: u64::from(access.to_byte()),
            incarnation_high: u64::try_from(incarnation >> 64).unwrap_or(u64::MAX),
            incarnation_low: u64::try_from(incarnation & u128::from(u64::MAX)).unwrap_or(0),
            index: token.index(),
            generation: u64::from(token.generation()),
            ip: IpBytes::from_ip(endpoint.address().ip()),
            port: u64::from(endpoint.address().port()),
        }
    }

    /// The endpoint as far as the fields allow; see [`Routing::check`].
    fn endpoint(&self) -> Endpoint {
        let ip = self.ip.ip().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let port = u16::try_from(self.port).unwrap_or(0);
        let incarnation =
            (u128::from(self.incarnation_high) << 64) | u128::from(self.incarnation_low);
        let generation = u32::try_from(self.generation).unwrap_or(u32::MAX);
        Endpoint::new(
            SocketAddr::new(ip, port),
            Incarnation::from_raw(incarnation),
            EndpointToken::from_parts(self.index, generation),
        )
    }

    fn access(&self) -> AccessClass {
        u8::try_from(self.access)
            .ok()
            .and_then(AccessClass::from_byte)
            .unwrap_or_default()
    }

    fn check(&self) -> Result<(), String> {
        if u8::try_from(self.access)
            .ok()
            .and_then(AccessClass::from_byte)
            .is_none()
        {
            return Err(format!("unknown access class {}", self.access));
        }
        let Some(ip) = self.ip.ip() else {
            return Err(format!(
                "address of {} bytes is neither IPv4 nor IPv6",
                self.ip.as_slice().len()
            ));
        };
        if ip.is_unspecified() {
            return Err("reference has no address".into());
        }
        match u16::try_from(self.port) {
            Ok(0) | Err(_) => return Err(format!("invalid port {}", self.port)),
            Ok(_) => {}
        }
        if u32::try_from(self.generation).is_err() {
            return Err(format!("invalid token generation {}", self.generation));
        }
        Ok(())
    }

    fn encode<S: Sink>(&self, out: &mut ProtoWriter<'_, S>) {
        out.varint(tags::ACCESS, self.access)
            .fixed64(tags::INCARNATION_HIGH, self.incarnation_high)
            .fixed64(tags::INCARNATION_LOW, self.incarnation_low)
            .fixed64(tags::TOKEN_INDEX, self.index)
            .varint(tags::TOKEN_GENERATION, self.generation)
            .bytes(tags::IP, self.ip.as_slice())
            .varint(tags::PORT, self.port);
    }

    fn kind(tag: u32) -> Option<FieldKind> {
        match tag {
            tags::ACCESS | tags::TOKEN_GENERATION | tags::PORT => Some(FieldKind::Varint),
            tags::INCARNATION_HIGH | tags::INCARNATION_LOW | tags::TOKEN_INDEX => {
                Some(FieldKind::Fixed64)
            }
            tags::IP => Some(FieldKind::Bytes),
            _ => None,
        }
    }

    fn apply(&mut self, tag: u32, value: Value<'_>) -> Result<(), WrongWireType> {
        match (tag, value) {
            (tags::ACCESS, Value::Varint(raw)) => self.access = raw,
            (tags::INCARNATION_HIGH, Value::Fixed64(raw)) => self.incarnation_high = raw,
            (tags::INCARNATION_LOW, Value::Fixed64(raw)) => self.incarnation_low = raw,
            (tags::TOKEN_INDEX, Value::Fixed64(raw)) => self.index = raw,
            (tags::TOKEN_GENERATION, Value::Varint(raw)) => self.generation = raw,
            (tags::IP, Value::Bytes(raw)) => self.ip = IpBytes::from_slice(raw),
            (tags::PORT, Value::Varint(raw)) => self.port = raw,
            (tag, _) if Self::kind(tag).is_some() => return Err(WrongWireType),
            // Unknown fields are skipped: a newer peer may add some.
            _ => {}
        }
        Ok(())
    }
}

/// One of the two reference messages, for the shared codec code.
pub(crate) trait RefMessage: Default {
    /// Append every field, in tag order, zeros included.
    fn encode_fields<S: Sink>(&self, out: &mut ProtoWriter<'_, S>);
    /// The shape of a known tag.
    fn field_kind(tag: u32) -> Option<FieldKind>;
    /// Merge one decoded field.
    fn apply(&mut self, tag: u32, value: Value<'_>) -> Result<(), WrongWireType>;

    fn to_proto(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_fields(&mut ProtoWriter(&mut out));
        out
    }

    /// The encoded length, measured without allocating.
    fn proto_len(&self) -> usize {
        let mut counter = Counter::default();
        self.encode_fields(&mut ProtoWriter(&mut counter));
        counter.0
    }

    /// Decode without judging the content (see each type's `check`).
    fn from_proto(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut value = Self::default();
        let mut reader = ProtoReader::new(bytes);
        while let Some((tag, field)) = reader
            .field()
            .map_err(|error: ProtoError| DecodeError(error.to_string()))?
        {
            if Self::field_kind(tag).is_none() {
                // Unknown fields are skipped: a newer peer may add some.
                continue;
            }
            value.apply(tag, field).map_err(|WrongWireType| {
                DecodeError(format!("field {tag} has the wrong wire type"))
            })?;
        }
        Ok(value)
    }
}

/// Field tags of the method identity in a [`ServiceRef`].
mod service_tags {
    pub(super) const METHOD: u32 = 1;
    pub(super) const SCHEMA: u32 = 2;
    pub(super) const REQUEST_CODEC: u32 = 3;
    pub(super) const REPLY_CODEC: u32 = 4;
    pub(super) const INTERFACE: u32 = 12;
    pub(super) const INTERFACE_VERSION: u32 = 13;
}

/// Field tags of the interface identity in an [`InterfaceRef`].
mod interface_tags {
    pub(super) const INTERFACE: u32 = 1;
    pub(super) const VERSION: u32 = 2;
}

/// A typed reference to one method of a dynamic endpoint.
///
/// Plain routing data: it keeps nothing alive, needs no runtime to exist,
/// be stored or be decoded, and **is itself a protobuf message**, so an
/// application embeds it as a field of its own messages, returns it in a
/// reply or writes it to disk:
///
/// ```
/// # #[cfg(feature = "prost")] {
/// use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion, ServiceRef};
///
/// /// A callback the server calls when the job finishes.
/// pub struct Notify;
/// impl RpcMethod for Notify {
///     type Request = String;
///     type Reply = ();
///     const METHOD: MethodId = MethodId::new(0x6e74_6679);
///     const SCHEMA: SchemaVersion = SchemaVersion::new(1);
///     const NAME: &'static str = "notify";
/// }
///
/// #[derive(Clone, PartialEq, prost::Message)]
/// pub struct StartJob {
///     #[prost(string, tag = "1")]
///     pub name: String,
///     /// Where to report completion: an ordinary, fully addressed endpoint.
///     #[prost(message, optional, tag = "2")]
///     pub on_done: Option<ServiceRef<Notify>>,
/// }
/// # }
/// ```
///
/// # Encoding
///
/// A protobuf message with fixed tags, written in tag order with fields
/// equal to their default left out (proto3, as prost does), so one
/// reference has exactly one encoding:
///
/// | Tag | Field | Type |
/// |---|---|---|
/// | 1 | method id | `uint32` |
/// | 2 | schema version | `uint32` (a `u16`) |
/// | 3 | request codec | `uint32` (a `u16`) |
/// | 4 | reply codec | `uint32` (a `u16`) |
/// | 5 | access class (0 private, 1 public) | `uint32` |
/// | 6, 7 | incarnation, high and low 64 bits | `fixed64` |
/// | 8 | token index (the top bit marks a well-known id) | `fixed64` |
/// | 9 | token generation | `uint32` |
/// | 10 | IP address, 4 or 16 bytes | `bytes` |
/// | 11 | port | `uint32` |
/// | 12 | interface id of the group (0: a single-method endpoint) | `uint32` |
/// | 13 | interface version | `uint32` (a `u16`) |
///
/// Unknown tags (groups included) are skipped. The encoding needs neither the `prost` feature
/// nor a runtime; with the feature, `ServiceRef` also implements
/// `prost::Message` (and therefore [`Wire`]).
///
/// # Checked use
///
/// [`from_bytes`](Self::from_bytes) decodes strictly: the method, schema
/// and both codecs must be `M`'s and the address well formed. Decoding as
/// a field of another message cannot reject content (protobuf merges
/// field by field), so such a reference is kept exactly as decoded and
/// [`check`](Self::check)ed before every use: a [`ServiceClient`] refuses
/// to send through a reference that fails it
/// ([`ErrorReason::InvalidReference`], never admitted), and re-encoding
/// never repairs it. The server independently validates the incarnation,
/// token, method, schema and codec of every request before any byte
/// reaches a handler.
pub struct ServiceRef<M: RpcMethod> {
    method: u64,
    schema: u64,
    request_codec: u64,
    reply_codec: u64,
    /// The interface of the group this method belongs to (0: a
    /// single-method endpoint).
    interface: u64,
    interface_version: u64,
    routing: Routing,
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
            method: u64::from(M::METHOD.get()),
            schema: u64::from(M::SCHEMA.get()),
            request_codec: u64::from(<M::Request as Wire>::CODEC.get()),
            reply_codec: u64::from(<M::Reply as Wire>::CODEC.get()),
            interface: 0,
            interface_version: 0,
            routing: Routing::new(&endpoint, access),
            _method: PhantomData,
        }
    }

    /// A reference to method `M` of a group serving interface `I`: the
    /// server checks the interface as well as the method.
    #[must_use]
    pub fn in_interface<I: RpcInterface>(endpoint: Endpoint, access: AccessClass) -> Self
    where
        M: InterfaceMethod<I>,
    {
        let mut reference = Self::new(endpoint, access);
        reference.interface = u64::from(I::INTERFACE.get());
        reference.interface_version = u64::from(I::VERSION.get());
        reference
    }

    /// The interface of the group this method belongs to and its version,
    /// or `None` for a single-method endpoint (or a malformed value, which
    /// [`check`](Self::check) reports).
    #[must_use]
    pub fn interface(&self) -> Option<(InterfaceId, SchemaVersion)> {
        let id = u32::try_from(self.interface).ok()?;
        let version = u16::try_from(self.interface_version).ok()?;
        (id != 0).then(|| (InterfaceId::new(id), SchemaVersion::new(version)))
    }

    /// The addressed endpoint (for a reference that fails
    /// [`check`](Self::check), as far as its fields allow).
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.routing.endpoint()
    }

    /// The access class the endpoint was registered with.
    #[must_use]
    pub fn access(&self) -> AccessClass {
        self.routing.access()
    }

    /// Whether this reference is well formed and names `M`'s method,
    /// schema and codecs.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] saying what is wrong.
    pub fn check(&self) -> Result<(), DecodeError> {
        let expected = (
            u64::from(M::METHOD.get()),
            u64::from(M::SCHEMA.get()),
            u64::from(<M::Request as Wire>::CODEC.get()),
            u64::from(<M::Reply as Wire>::CODEC.get()),
        );
        let actual = (
            self.method,
            self.schema,
            self.request_codec,
            self.reply_codec,
        );
        if actual != expected {
            return Err(DecodeError(format!(
                "service reference serves method {:#x} schema {} ({:#x}->{:#x}), not {} ({}/{})",
                actual.0,
                actual.1,
                actual.2,
                actual.3,
                M::NAME,
                M::METHOD,
                M::SCHEMA
            )));
        }
        if u32::try_from(self.interface).is_err()
            || u16::try_from(self.interface_version).is_err()
            || (self.interface == 0 && self.interface_version != 0)
        {
            return Err(DecodeError(format!(
                "invalid interface {:#x} version {}",
                self.interface, self.interface_version
            )));
        }
        self.routing.check().map_err(DecodeError)
    }

    /// Bind to a runtime, producing a client that calls through it.
    ///
    /// Binding registers nothing and checks nothing remote: it only
    /// chooses the runtime whose connections carry the calls.
    #[must_use]
    pub fn bind<P: Providers>(&self, rpc: &RpcHandle<P>) -> ServiceClient<P, M> {
        ServiceClient::new(rpc.clone(), self.clone())
    }

    /// The reference's protobuf encoding (see the type docs).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_proto()
    }

    /// Decode a reference strictly: well formed, and serving `M`.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] for malformed protobuf, or a reference that fails
    /// [`check`](Self::check).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let reference = Self::from_proto(bytes)?;
        reference.check()?;
        Ok(reference)
    }
}

impl<M: RpcMethod> Default for ServiceRef<M> {
    /// An empty reference (no method, no address): it fails
    /// [`check`](Self::check). Protobuf decoding starts from it.
    fn default() -> Self {
        Self {
            method: 0,
            schema: 0,
            request_codec: 0,
            reply_codec: 0,
            interface: 0,
            interface_version: 0,
            routing: Routing::default(),
            _method: PhantomData,
        }
    }
}

impl<M: RpcMethod> RefMessage for ServiceRef<M> {
    fn encode_fields<S: Sink>(&self, out: &mut ProtoWriter<'_, S>) {
        out.varint(service_tags::METHOD, self.method)
            .varint(service_tags::SCHEMA, self.schema)
            .varint(service_tags::REQUEST_CODEC, self.request_codec)
            .varint(service_tags::REPLY_CODEC, self.reply_codec);
        self.routing.encode(out);
        out.varint(service_tags::INTERFACE, self.interface)
            .varint(service_tags::INTERFACE_VERSION, self.interface_version);
    }

    fn field_kind(tag: u32) -> Option<FieldKind> {
        match tag {
            service_tags::METHOD
            | service_tags::SCHEMA
            | service_tags::REQUEST_CODEC
            | service_tags::REPLY_CODEC
            | service_tags::INTERFACE
            | service_tags::INTERFACE_VERSION => Some(FieldKind::Varint),
            tag => Routing::kind(tag),
        }
    }

    fn apply(&mut self, tag: u32, value: Value<'_>) -> Result<(), WrongWireType> {
        match (tag, value) {
            (service_tags::METHOD, Value::Varint(raw)) => self.method = raw,
            (service_tags::SCHEMA, Value::Varint(raw)) => self.schema = raw,
            (service_tags::REQUEST_CODEC, Value::Varint(raw)) => self.request_codec = raw,
            (service_tags::REPLY_CODEC, Value::Varint(raw)) => self.reply_codec = raw,
            (service_tags::INTERFACE, Value::Varint(raw)) => self.interface = raw,
            (service_tags::INTERFACE_VERSION, Value::Varint(raw)) => self.interface_version = raw,
            (
                service_tags::METHOD
                | service_tags::SCHEMA
                | service_tags::REQUEST_CODEC
                | service_tags::REPLY_CODEC
                | service_tags::INTERFACE
                | service_tags::INTERFACE_VERSION,
                _,
            ) => return Err(WrongWireType),
            (tag, value) => return self.routing.apply(tag, value),
        }
        Ok(())
    }
}

impl<M: RpcMethod> Clone for ServiceRef<M> {
    fn clone(&self) -> Self {
        Self {
            method: self.method,
            schema: self.schema,
            request_codec: self.request_codec,
            reply_codec: self.reply_codec,
            interface: self.interface,
            interface_version: self.interface_version,
            routing: self.routing.clone(),
            _method: PhantomData,
        }
    }
}

impl<M: RpcMethod> PartialEq for ServiceRef<M> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl<M: RpcMethod> Eq for ServiceRef<M> {}

impl<M: RpcMethod> std::hash::Hash for ServiceRef<M> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl<M: RpcMethod> PartialOrd for ServiceRef<M> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<M: RpcMethod> Ord for ServiceRef<M> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl<M: RpcMethod> ServiceRef<M> {
    fn key(&self) -> ([u64; 6], &Routing) {
        (
            [
                self.method,
                self.schema,
                self.request_codec,
                self.reply_codec,
                self.interface,
                self.interface_version,
            ],
            &self.routing,
        )
    }
}

impl<M: RpcMethod> std::fmt::Debug for ServiceRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ServiceRef");
        debug
            .field("method", &M::NAME)
            .field("endpoint", &self.endpoint())
            .field("access", &self.access());
        if let Err(error) = self.check() {
            debug.field("invalid", &error.0);
        }
        debug.finish()
    }
}

/// A typed reference to a dynamic endpoint group serving interface `I`.
///
/// One registry slot, one incarnation, several methods: the group-level
/// counterpart of [`ServiceRef`] and, like it, a protobuf message an
/// application embeds, returns, forwards or stores. A caller adjusts it to
/// one method with [`method`](Self::method): the result addresses the same
/// endpoint in the same incarnation and differs only in its explicit
/// [`MethodId`](crate::MethodId), so reordering, adding or removing methods in the source
/// never retargets an existing reference; a method the group does not
/// serve is refused by the server ([`ErrorReason::MethodNotFound`]).
///
/// # Encoding
///
/// | Tag | Field | Type |
/// |---|---|---|
/// | 1 | interface id | `uint32` |
/// | 2 | interface version | `uint32` (a `u16`) |
/// | 5..=11 | routing, as in [`ServiceRef`] | |
///
/// Tags 3 and 4 are reserved. Checking follows [`ServiceRef`]:
/// [`from_bytes`](Self::from_bytes) is strict, a reference decoded inside
/// another message is [`check`](Self::check)ed by
/// [`method`](Self::method) and [`bind`](Self::bind).
pub struct InterfaceRef<I: RpcInterface> {
    interface: u64,
    version: u64,
    routing: Routing,
    _interface: PhantomData<fn() -> I>,
}

impl<I: RpcInterface> InterfaceRef<I> {
    /// Claim that `endpoint` is a group serving `I`.
    #[must_use]
    pub fn new(endpoint: Endpoint, access: AccessClass) -> Self {
        Self {
            interface: u64::from(I::INTERFACE.get()),
            version: u64::from(I::VERSION.get()),
            routing: Routing::new(&endpoint, access),
            _interface: PhantomData,
        }
    }

    /// The group's endpoint.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.routing.endpoint()
    }

    /// The access class the group was registered with.
    #[must_use]
    pub fn access(&self) -> AccessClass {
        self.routing.access()
    }

    /// Whether this reference is well formed and names `I`.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] saying what is wrong.
    pub fn check(&self) -> Result<(), DecodeError> {
        let expected = (u64::from(I::INTERFACE.get()), u64::from(I::VERSION.get()));
        if (self.interface, self.version) != expected {
            return Err(DecodeError(format!(
                "interface reference names interface {:#x} version {}, not {} ({}/{})",
                self.interface,
                self.version,
                I::NAME,
                I::INTERFACE,
                I::VERSION
            )));
        }
        self.routing.check().map_err(DecodeError)
    }

    /// Adjust to method `M` of the group: same address, incarnation and
    /// token; `M`'s explicit method identity.
    ///
    /// # Errors
    ///
    /// The reference fails [`check`](Self::check).
    pub fn method<M: InterfaceMethod<I>>(&self) -> Result<ServiceRef<M>, DecodeError> {
        self.check()?;
        Ok(ServiceRef::in_interface::<I>(
            self.endpoint(),
            self.access(),
        ))
    }

    /// Bind to a runtime after [`check`](Self::check)ing the reference.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::InvalidReference`] (never admitted) when the
    /// reference fails its check.
    pub fn bind<P: Providers>(
        &self,
        rpc: &RpcHandle<P>,
    ) -> Result<InterfaceClient<P, I>, RpcError> {
        self.check()
            .map_err(|error| RpcError::not_admitted(ErrorReason::InvalidReference(error.0)))?;
        Ok(InterfaceClient {
            rpc: rpc.clone(),
            target: self.clone(),
        })
    }

    /// The reference's protobuf encoding (see the type docs).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_proto()
    }

    /// Decode a reference strictly: well formed, and naming `I`.
    ///
    /// # Errors
    ///
    /// A [`DecodeError`] for malformed protobuf, or a reference that fails
    /// [`check`](Self::check).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let reference = Self::from_proto(bytes)?;
        reference.check()?;
        Ok(reference)
    }

    /// The interface id as decoded.
    #[must_use]
    pub fn interface_id(&self) -> Option<InterfaceId> {
        u32::try_from(self.interface).ok().map(InterfaceId::new)
    }
}

impl<I: RpcInterface> Default for InterfaceRef<I> {
    /// An empty reference: it fails [`check`](Self::check).
    fn default() -> Self {
        Self {
            interface: 0,
            version: 0,
            routing: Routing::default(),
            _interface: PhantomData,
        }
    }
}

impl<I: RpcInterface> RefMessage for InterfaceRef<I> {
    fn encode_fields<S: Sink>(&self, out: &mut ProtoWriter<'_, S>) {
        out.varint(interface_tags::INTERFACE, self.interface)
            .varint(interface_tags::VERSION, self.version);
        self.routing.encode(out);
    }

    fn field_kind(tag: u32) -> Option<FieldKind> {
        match tag {
            interface_tags::INTERFACE | interface_tags::VERSION => Some(FieldKind::Varint),
            tag => Routing::kind(tag),
        }
    }

    fn apply(&mut self, tag: u32, value: Value<'_>) -> Result<(), WrongWireType> {
        match (tag, value) {
            (interface_tags::INTERFACE, Value::Varint(raw)) => self.interface = raw,
            (interface_tags::VERSION, Value::Varint(raw)) => self.version = raw,
            (interface_tags::INTERFACE | interface_tags::VERSION, _) => {
                return Err(WrongWireType);
            }
            (tag, value) => return self.routing.apply(tag, value),
        }
        Ok(())
    }
}

impl<I: RpcInterface> Clone for InterfaceRef<I> {
    fn clone(&self) -> Self {
        Self {
            interface: self.interface,
            version: self.version,
            routing: self.routing.clone(),
            _interface: PhantomData,
        }
    }
}

impl<I: RpcInterface> InterfaceRef<I> {
    fn key(&self) -> (u64, u64, &Routing) {
        (self.interface, self.version, &self.routing)
    }
}

impl<I: RpcInterface> PartialEq for InterfaceRef<I> {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl<I: RpcInterface> Eq for InterfaceRef<I> {}

impl<I: RpcInterface> std::hash::Hash for InterfaceRef<I> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl<I: RpcInterface> PartialOrd for InterfaceRef<I> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<I: RpcInterface> Ord for InterfaceRef<I> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl<I: RpcInterface> std::fmt::Debug for InterfaceRef<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("InterfaceRef");
        debug
            .field("interface", &I::NAME)
            .field("endpoint", &self.endpoint())
            .field("access", &self.access());
        if let Err(error) = self.check() {
            debug.field("invalid", &error.0);
        }
        debug.finish()
    }
}

/// An [`InterfaceRef`] bound to a runtime: hands out one
/// [`ServiceClient`] per method, each with every delivery mode.
pub struct InterfaceClient<P: Providers, I: RpcInterface> {
    rpc: RpcHandle<P>,
    target: InterfaceRef<I>,
}

impl<P: Providers, I: RpcInterface> InterfaceClient<P, I> {
    /// The reference this client calls.
    #[must_use]
    pub fn target(&self) -> &InterfaceRef<I> {
        &self.target
    }

    /// A client of method `M` of the group.
    #[must_use]
    pub fn method<M: InterfaceMethod<I>>(&self) -> ServiceClient<P, M> {
        // The target was checked when it was bound.
        ServiceRef::in_interface::<I>(self.target.endpoint(), self.target.access()).bind(&self.rpc)
    }
}

impl<P: Providers, I: RpcInterface> Clone for InterfaceClient<P, I> {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            target: self.target.clone(),
        }
    }
}

impl<P: Providers, I: RpcInterface> std::fmt::Debug for InterfaceClient<P, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterfaceClient")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// `Wire` for references without the `prost` feature; with it, the blanket
/// impl over `prost::Message` applies and produces the same bytes.
#[cfg(not(feature = "prost"))]
mod plain_wire {
    use super::{InterfaceRef, RefMessage, ServiceRef};
    use crate::codec::{CodecId, DecodeError, EncodeError, Wire};
    use crate::interface::RpcInterface;
    use crate::interface::proto::ProtoWriter;
    use crate::protocol::RpcMethod;

    impl<M: RpcMethod> Wire for ServiceRef<M> {
        const CODEC: CodecId = CodecId::PROST;

        fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
            buf.reserve(self.proto_len());
            self.encode_fields(&mut ProtoWriter(buf));
            Ok(())
        }

        /// Lenient like protobuf decoding: checked before use.
        fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
            Self::from_proto(bytes)
        }
    }

    impl<I: RpcInterface> Wire for InterfaceRef<I> {
        const CODEC: CodecId = CodecId::PROST;

        fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
            buf.reserve(self.proto_len());
            self.encode_fields(&mut ProtoWriter(buf));
            Ok(())
        }

        fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
            Self::from_proto(bytes)
        }
    }
}

/// `prost::Message` for references: the same bytes as the plain codec,
/// decoded field by field through prost's own helpers so its errors,
/// recursion limits and unknown-field skipping apply.
#[cfg(feature = "prost")]
mod prost_message {
    use prost::bytes::{Buf, BufMut};
    use prost::encoding::{DecodeContext, WireType};

    use super::{FieldKind, InterfaceRef, RefMessage, ServiceRef};
    use crate::interface::RpcInterface;
    use crate::interface::proto::{ProtoWriter, Sink, Value};

    /// Writes straight into prost's buffer.
    struct BufSink<'a, B: BufMut>(&'a mut B);

    impl<B: BufMut> Sink for BufSink<'_, B> {
        fn put(&mut self, bytes: &[u8]) {
            self.0.put_slice(bytes);
        }
    }
    use crate::protocol::RpcMethod;

    fn merge<T: RefMessage>(
        message: &mut T,
        tag: u32,
        wire_type: WireType,
        buf: &mut impl Buf,
        ctx: DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        let applied = match T::field_kind(tag) {
            None => return prost::encoding::skip_field(wire_type, tag, buf, ctx),
            Some(FieldKind::Varint) => {
                let mut raw = 0u64;
                prost::encoding::uint64::merge(wire_type, &mut raw, buf, ctx)?;
                message.apply(tag, Value::Varint(raw))
            }
            Some(FieldKind::Fixed64) => {
                let mut raw = 0u64;
                prost::encoding::fixed64::merge(wire_type, &mut raw, buf, ctx)?;
                message.apply(tag, Value::Fixed64(raw))
            }
            Some(FieldKind::Bytes) => {
                let mut raw = Vec::new();
                prost::encoding::bytes::merge(wire_type, &mut raw, buf, ctx)?;
                message.apply(tag, Value::Bytes(&raw))
            }
        };
        // The typed helpers above already refused a wrong wire type.
        debug_assert!(applied.is_ok());
        Ok(())
    }

    macro_rules! reference_message {
        ($type:ident, $param:ident, $bound:path) => {
            impl<$param: $bound> prost::Message for $type<$param> {
                fn encode_raw(&self, buf: &mut impl BufMut) {
                    self.encode_fields(&mut ProtoWriter(&mut BufSink(buf)));
                }

                fn merge_field(
                    &mut self,
                    tag: u32,
                    wire_type: WireType,
                    buf: &mut impl Buf,
                    ctx: DecodeContext,
                ) -> Result<(), prost::DecodeError> {
                    merge(self, tag, wire_type, buf, ctx)
                }

                fn encoded_len(&self) -> usize {
                    self.proto_len()
                }

                fn clear(&mut self) {
                    *self = Self::default();
                }
            }
        };
    }

    reference_message!(ServiceRef, M, RpcMethod);
    reference_message!(InterfaceRef, I, RpcInterface);
}
