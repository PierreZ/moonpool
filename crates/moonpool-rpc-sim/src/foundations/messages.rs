//! The campaign's messages and methods. Every identifier is an explicit
//! constant, never derived from a Rust name.

use moonpool_rpc::{CodecId, DecodeError, EncodeError, MethodId, RpcMethod, SchemaVersion, Wire};

/// A request: the workload-generated id and a body derived from it, so a
/// handler can prove it received exactly what was sent.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Workload-generated request id, unique per run.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// Always [`probe_text`] of `id`.
    #[prost(string, tag = "2")]
    pub text: String,
}

impl Probe {
    /// The probe for request `id`.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            text: probe_text(id),
        }
    }

    /// Whether the body is exactly what the workload sent for this id.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.text == probe_text(self.id)
    }
}

/// The body the workload sends for request `id`.
#[must_use]
pub fn probe_text(id: u64) -> String {
    format!("probe-{id:016x}")
}

/// A reply from a server handler.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Echoed {
    /// The request id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// The request body, echoed.
    #[prost(string, tag = "2")]
    pub text: String,
}

/// The relay's report of its own single forwarded attempt.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Relayed {
    /// The request id.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// What the relay's call to the server proved: see [`RelayOutcome`].
    #[prost(uint32, tag = "2")]
    pub outcome: u32,
    /// The server's echoed text on success.
    #[prost(string, tag = "3")]
    pub text: String,
}

/// Values of [`Relayed::outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayOutcome {
    /// The server replied.
    Replied = 0,
    /// The server never admitted the request.
    NotAdmitted = 1,
    /// The server may have executed it.
    MaybeExecuted = 2,
    /// The server executed it but the reply was unusable.
    Executed = 3,
}

impl RelayOutcome {
    /// Decode the wire value.
    #[must_use]
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Replied),
            1 => Some(Self::NotAdmitted),
            2 => Some(Self::MaybeExecuted),
            3 => Some(Self::Executed),
            _ => None,
        }
    }
}

macro_rules! method {
    ($(#[$doc:meta])* $name:ident, $request:ty, $reply:ty, $method:expr, $schema:expr, $label:expr) => {
        $(#[$doc])*
        pub struct $name;

        impl RpcMethod for $name {
            type Request = $request;
            type Reply = $reply;
            const METHOD: MethodId = MethodId::new($method);
            const SCHEMA: SchemaVersion = SchemaVersion::new($schema);
            const NAME: &'static str = $label;
        }
    };
}

method!(
    /// Echo the probe back at once.
    Echo, Probe, Echoed, 0x0100, 1, "echo"
);
method!(
    /// Echo after a provider-time delay (cancellation and late replies).
    Slow, Probe, Echoed, 0x0101, 1, "slow"
);
method!(
    /// Served once, then its endpoint is destroyed and a fresh one
    /// registered in the same registry slot.
    Ephemeral, Probe, Echoed, 0x0102, 1, "ephemeral"
);
method!(
    /// Recorded, never answered; asks the fault script to crash the server.
    CrashAfterReceipt, Probe, Echoed, 0x0103, 1, "crash-after-receipt"
);
method!(
    /// Forwarded by the relay process to the server's echo endpoint.
    Relay, Probe, Relayed, 0x0200, 1, "relay"
);
method!(
    /// Echo's method with a newer contract version the server never
    /// registered.
    EchoNextSchema, Probe, Echoed, 0x0100, 2, "echo.v2"
);
method!(
    /// A method id the server never registered, aimed at echo's endpoint.
    WrongMethod, Probe, Echoed, 0x01FF, 1, "wrong-method"
);
method!(
    /// Echo's method and schema with a body in a foreign codec.
    EchoForeignCodec, ForeignBody, Echoed, 0x0100, 1, "echo.foreign-codec"
);

/// Bytes tagged with an application codec id the server never registered.
pub struct ForeignBody(pub Vec<u8>);

impl Wire for ForeignBody {
    const CODEC: CodecId = CodecId::new(0x8123);

    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
        buf.extend_from_slice(&self.0);
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(bytes.to_vec()))
    }
}
