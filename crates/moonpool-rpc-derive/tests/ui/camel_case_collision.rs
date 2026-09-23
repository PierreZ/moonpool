#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(id = 1, schema = 1)]
    async fn put_many(&self, entry: String);
    #[method(id = 2, schema = 1)]
    async fn put__many(&self, entry: String);
}

fn main() {}
