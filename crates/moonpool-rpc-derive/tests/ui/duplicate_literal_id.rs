#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(id = 7, schema = 1)]
    async fn get(&self, key: String) -> String;
    #[method(id = 7, schema = 1)]
    async fn put(&self, entry: String);
}

fn main() {}
