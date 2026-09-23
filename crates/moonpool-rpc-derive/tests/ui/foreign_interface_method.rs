use moonpool_rpc::{InterfaceId, InterfaceRef, MethodId, RpcInterface, RpcMethod, SchemaVersion};

struct Kv;
impl RpcInterface for Kv {
    const INTERFACE: InterfaceId = InterfaceId::new(1);
    const VERSION: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "kv";
}

/// A method of no interface at all, with a matching id.
struct Other;
impl RpcMethod for Other {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "other";
}

fn main() {
    let _ = InterfaceRef::<Kv>::default().method::<Other>();
}
