//! References as data: golden encodings, protobuf nesting, stored blobs,
//! strict and lenient decoding, and checked method adjustment. No runtime
//! is involved: a reference decodes without one.

#![cfg(feature = "prost")]

use std::net::SocketAddr;

use moonpool_rpc::{
    AccessClass, Endpoint, EndpointToken, Incarnation, InterfaceId, InterfaceMethod, InterfaceRef,
    MethodId, RpcInterface, RpcMethod, SchemaVersion, ServiceRef, WellKnownId,
};
use prost::Message;

/// A method whose id matches the golden fixture.
struct Echo;
impl RpcMethod for Echo {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(0x6563_686f);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

/// Same id, next contract version.
struct EchoV2;
impl RpcMethod for EchoV2 {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(0x6563_686f);
    const SCHEMA: SchemaVersion = SchemaVersion::new(2);
    const NAME: &'static str = "echo.v2";
}

/// Another method with the same shapes.
struct Shout;
impl RpcMethod for Shout {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(0x5307);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "shout";
}

/// The interface of the golden fixture: `Echo` and `Shout`.
struct Kv;
impl RpcInterface for Kv {
    const INTERFACE: InterfaceId = InterfaceId::new(0x6b76_0001);
    const VERSION: SchemaVersion = SchemaVersion::new(2);
    const NAME: &'static str = "kv";
}
impl InterfaceMethod<Kv> for Echo {}
impl InterfaceMethod<Kv> for Shout {}

/// A different interface.
struct Other;
impl RpcInterface for Other {
    const INTERFACE: InterfaceId = InterfaceId::new(0x6b76_0002);
    const VERSION: SchemaVersion = SchemaVersion::new(2);
    const NAME: &'static str = "other";
}

/// The same interface declared again with its methods in the opposite
/// order and new Rust names, as a later source revision might: the wire
/// meaning of a stored reference cannot change, because only the explicit
/// ids travel.
mod reordered {
    use moonpool_rpc::{MethodId, RpcMethod, SchemaVersion};

    pub struct Loud;
    impl RpcMethod for Loud {
        type Request = String;
        type Reply = String;
        const METHOD: MethodId = MethodId::new(0x5307);
        const SCHEMA: SchemaVersion = SchemaVersion::new(1);
        const NAME: &'static str = "loud";
    }
    impl moonpool_rpc::InterfaceMethod<super::Kv> for Loud {}

    pub struct Repeat;
    impl RpcMethod for Repeat {
        type Request = String;
        type Reply = String;
        const METHOD: MethodId = MethodId::new(0x6563_686f);
        const SCHEMA: SchemaVersion = SchemaVersion::new(1);
        const NAME: &'static str = "repeat";
    }
    impl moonpool_rpc::InterfaceMethod<super::Kv> for Repeat {}
}

const INCARNATION: u128 = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10;

fn fixture(name: &str) -> Vec<u8> {
    let text = include_str!("fixtures/references-v1.txt");
    let hex = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("fixture {name} missing"));
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

fn address(text: &str) -> SocketAddr {
    text.parse().expect("address")
}

fn echo_ref() -> ServiceRef<Echo> {
    ServiceRef::new(
        Endpoint::new(
            address("10.0.1.1:4500"),
            Incarnation::from_raw(INCARNATION),
            EndpointToken::from_parts(3, 7),
        ),
        AccessClass::Public,
    )
}

fn kv_ref() -> InterfaceRef<Kv> {
    InterfaceRef::new(
        Endpoint::new(
            address("[2001:db8::1]:4500"),
            Incarnation::from_raw(INCARNATION),
            EndpointToken::from_parts(5, 1),
        ),
        AccessClass::Public,
    )
}

/// An independent protobuf description of `ServiceRef`, derived by prost:
/// both implementations must agree on every byte.
#[derive(Clone, PartialEq, prost::Message)]
struct ServiceRefMirror {
    #[prost(uint32, tag = "1")]
    method: u32,
    #[prost(uint32, tag = "2")]
    schema: u32,
    #[prost(uint32, tag = "3")]
    request_codec: u32,
    #[prost(uint32, tag = "4")]
    reply_codec: u32,
    #[prost(uint32, tag = "5")]
    access: u32,
    #[prost(fixed64, tag = "6")]
    incarnation_high: u64,
    #[prost(fixed64, tag = "7")]
    incarnation_low: u64,
    #[prost(fixed64, tag = "8")]
    token_index: u64,
    #[prost(uint32, tag = "9")]
    token_generation: u32,
    #[prost(bytes = "vec", tag = "10")]
    ip: Vec<u8>,
    #[prost(uint32, tag = "11")]
    port: u32,
}

#[test]
fn golden_encodings_are_fixed() {
    assert_eq!(echo_ref().to_bytes(), fixture("service"));
    assert_eq!(
        ServiceRef::<Echo>::from_bytes(&fixture("service")).expect("decodes"),
        echo_ref()
    );

    let well_known = moonpool_rpc::WellKnownRef::<Echo>::new(
        moonpool_rpc::BootstrapAddress::Resolved(address("10.0.1.1:4500")),
        WellKnownId::new(42),
        AccessClass::Private,
    )
    .at(address("10.0.1.1:4500"));
    assert_eq!(well_known.to_bytes(), fixture("well-known"));
    let decoded = ServiceRef::<Echo>::from_bytes(&fixture("well-known")).expect("decodes");
    let token = decoded.endpoint().token();
    assert!(token.is_well_known(), "the top index bit survives");
    assert_eq!(token.well_known_id(), Some(WellKnownId::new(42)));
    assert_eq!(decoded.endpoint().incarnation(), Incarnation::from_raw(0));

    assert_eq!(kv_ref().to_bytes(), fixture("interface"));
    let interface = InterfaceRef::<Kv>::from_bytes(&fixture("interface")).expect("decodes");
    assert_eq!(interface, kv_ref());
    assert_eq!(
        interface.endpoint().address(),
        address("[2001:db8::1]:4500")
    );
    assert_eq!(interface.interface_id(), Some(Kv::INTERFACE));
}

#[test]
fn hand_codec_and_prost_derive_agree() {
    let mirror = <ServiceRefMirror as Message>::decode(fixture("service").as_slice())
        .expect("prost decodes");
    assert_eq!(mirror.method, 0x6563_686f);
    assert_eq!(mirror.port, 4500);
    // No field of this fixture is zero, so prost writes the same bytes.
    assert_eq!(mirror.encode_to_vec(), fixture("service"));
    // prost omits zero fields, the reference writes every field: decode in
    // both directions and compare values rather than bytes.
    assert_eq!(
        ServiceRef::<Echo>::from_bytes(&mirror.encode_to_vec()).expect("decodes"),
        echo_ref()
    );
    let wk = <ServiceRefMirror as Message>::decode(fixture("well-known").as_slice())
        .expect("prost decodes");
    assert_eq!(wk.token_index, (1 << 63) | 0x2a);
    assert_eq!((wk.incarnation_high, wk.incarnation_low), (0, 0));
    // `prost::Message` on the reference itself writes the golden bytes.
    assert_eq!(Message::encode_to_vec(&echo_ref()), fixture("service"));
    assert_eq!(Message::encoded_len(&echo_ref()), fixture("service").len());
    assert_eq!(
        <ServiceRef<Echo> as moonpool_rpc::Wire>::CODEC,
        moonpool_rpc::CodecId::PROST
    );
}

/// An application message carrying references, as a request, a reply or a
/// stored record would.
#[derive(Clone, PartialEq, prost::Message)]
struct Recruited {
    #[prost(uint64, tag = "1")]
    configuration: u64,
    #[prost(message, optional, tag = "2")]
    echo: Option<ServiceRef<Echo>>,
    #[prost(message, repeated, tag = "3")]
    replicas: Vec<InterfaceRef<Kv>>,
}

#[test]
fn references_nest_in_messages_and_survive_storage() {
    let record = Recruited {
        configuration: 9,
        echo: Some(echo_ref()),
        replicas: vec![kv_ref(), kv_ref()],
    };
    // A stored blob: bytes on disk, decoded later with no runtime.
    let blob = record.encode_to_vec();
    let restored = <Recruited as Message>::decode(blob.as_slice()).expect("decodes");
    assert_eq!(restored, record);
    let echo = restored.echo.expect("present");
    assert!(echo.check().is_ok());
    assert_eq!(echo.endpoint(), echo_ref().endpoint());
    assert_eq!(
        <Recruited as moonpool_rpc::Wire>::decode(&blob).expect("wire"),
        record
    );
}

#[test]
fn strict_decoding_refuses_the_wrong_type_and_malformed_routing() {
    let bytes = echo_ref().to_bytes();
    assert!(ServiceRef::<EchoV2>::from_bytes(&bytes).is_err(), "schema");
    assert!(ServiceRef::<Shout>::from_bytes(&bytes).is_err(), "method");
    assert!(InterfaceRef::<Other>::from_bytes(&kv_ref().to_bytes()).is_err());
    assert!(ServiceRef::<Echo>::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut mirror = <ServiceRefMirror as Message>::decode(bytes.as_slice()).expect("decodes");
    for broken in [
        ServiceRefMirror {
            ip: vec![1, 2, 3],
            ..mirror.clone()
        },
        ServiceRefMirror {
            port: 0,
            ..mirror.clone()
        },
        ServiceRefMirror {
            port: 70_000,
            ..mirror.clone()
        },
        ServiceRefMirror {
            access: 9,
            ..mirror.clone()
        },
        ServiceRefMirror {
            ip: vec![0, 0, 0, 0],
            ..mirror.clone()
        },
        ServiceRefMirror {
            request_codec: 0x8001,
            ..mirror.clone()
        },
    ] {
        assert!(
            ServiceRef::<Echo>::from_bytes(&broken.encode_to_vec()).is_err(),
            "{broken:?}"
        );
    }
    // A field with the wrong wire type is malformed protobuf on both paths.
    let wrong_type = [0x0d, 1, 2, 3, 4];
    assert!(ServiceRef::<Echo>::from_bytes(&wrong_type).is_err());
    assert!(<ServiceRef<Echo> as Message>::decode(&wrong_type[..]).is_err());
    // Unknown fields are skipped on both paths.
    mirror.port = 4500;
    let mut extended = mirror.encode_to_vec();
    extended.extend([0xa0, 0x01, 0x07]); // field 20, varint 7
    assert_eq!(
        ServiceRef::<Echo>::from_bytes(&extended).expect("skips"),
        echo_ref()
    );
    assert_eq!(
        <ServiceRef<Echo> as Message>::decode(extended.as_slice()).expect("skips"),
        echo_ref()
    );
}

#[test]
fn a_nested_wrong_type_is_kept_as_decoded_and_never_repaired() {
    // A `Shout` reference smuggled into a field typed `ServiceRef<Echo>`:
    // protobuf decoding cannot refuse it, so it is kept exactly as decoded,
    // fails its check, and re-encodes to the same (still wrong) bytes.
    #[derive(Clone, PartialEq, prost::Message)]
    struct Carrier {
        #[prost(message, optional, tag = "1")]
        target: Option<ServiceRef<Shout>>,
    }
    let shout = ServiceRef::<Shout>::new(echo_ref().endpoint(), AccessClass::Public);
    let bytes = Carrier {
        target: Some(shout.clone()),
    }
    .encode_to_vec();
    let smuggled = <Recruited as Message>::decode(
        [
            vec![0x12, u8::try_from(shout.to_bytes().len()).expect("small")],
            shout.to_bytes(),
        ]
        .concat()
        .as_slice(),
    )
    .expect("protobuf merges field by field")
    .echo
    .expect("present");
    assert!(smuggled.check().is_err());
    assert!(format!("{smuggled:?}").contains("invalid"));
    assert_eq!(smuggled.to_bytes(), shout.to_bytes(), "not repaired");
    assert!(ServiceRef::<Echo>::from_bytes(&smuggled.to_bytes()).is_err());
    assert_eq!(
        <Carrier as Message>::decode(bytes.as_slice())
            .expect("decodes")
            .target,
        Some(shout)
    );
}

#[test]
fn adjustment_keeps_the_incarnation_and_uses_explicit_ids() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Carrier {
        #[prost(message, optional, tag = "1")]
        kv: Option<InterfaceRef<Kv>>,
    }
    let group = kv_ref();
    let echo = group.method::<Echo>().expect("adjusts");
    let shout = group.method::<Shout>().expect("adjusts");
    assert_eq!(echo.endpoint(), group.endpoint());
    assert_eq!(shout.endpoint(), group.endpoint());
    assert_eq!(
        echo.endpoint().incarnation(),
        Incarnation::from_raw(INCARNATION)
    );
    // Reordered and renamed declarations address the same methods.
    let repeat = group.method::<reordered::Repeat>().expect("adjusts");
    let loud = group.method::<reordered::Loud>().expect("adjusts");
    assert_eq!(repeat.to_bytes(), echo.to_bytes());
    assert_eq!(loud.to_bytes(), shout.to_bytes());
    assert!(ServiceRef::<Shout>::from_bytes(&loud.to_bytes()).is_ok());
    assert!(ServiceRef::<Echo>::from_bytes(&loud.to_bytes()).is_err());
    // A reference to another interface (or a defective one) never adjusts.
    let foreign = InterfaceRef::<Other>::new(group.endpoint(), AccessClass::Public).to_bytes();
    let as_kv = <Carrier as Message>::decode(
        [
            vec![0x0a, u8::try_from(foreign.len()).expect("small")],
            foreign,
        ]
        .concat()
        .as_slice(),
    )
    .expect("merges")
    .kv
    .expect("present");
    assert!(as_kv.method::<Echo>().is_err());
    assert!(InterfaceRef::<Kv>::default().method::<Echo>().is_err());
}
