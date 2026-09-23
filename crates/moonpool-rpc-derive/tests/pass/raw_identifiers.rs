use moonpool_rpc::RpcMethod;

#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(id = 1, schema = 1)]
    async fn r#type(&self, key: String) -> String;
    #[method(id = 2, schema = 1)]
    async fn r#match(&self, key: String);
}

fn main() {
    assert_eq!(KvType::NAME, "Kv.type");
    assert_eq!(KvMatch::METHOD.get(), 2);
}
