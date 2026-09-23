const GET: u32 = 1;

#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(id = GET, schema = 1)]
    async fn get(&self, key: String) -> String;
    #[method(id = 1, schema = 1)]
    async fn put(&self, entry: String);
}

fn main() {}
