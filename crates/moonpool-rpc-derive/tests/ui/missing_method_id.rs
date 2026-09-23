#[moonpool_rpc::service(id = 1, version = 1)]
pub trait Kv {
    #[method(schema = 1)]
    async fn get(&self, key: String) -> String;
}

fn main() {}
