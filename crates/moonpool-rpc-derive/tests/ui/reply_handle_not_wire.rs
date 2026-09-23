use moonpool_rpc::{MethodId, ReplyHandle, RpcMethod, SchemaVersion, Wire};

struct Echo;
impl RpcMethod for Echo {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

fn embed<T: Wire>() {}

fn main() {
    // A reply handle is bound to its session: it is not data.
    embed::<ReplyHandle<Echo>>();
}
