#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(id = 1, schema = 1)]
    async fn bind(&self, key: String) -> String;
}

fn main() {}
