# moonpool-rpc-derive

The optional `#[moonpool_rpc::service]` attribute. From one annotated trait,
it generates a typed `moonpool-rpc` interface:

- the interface marker and one method marker per method, with their
  explicit ids;
- a request enum and a server (`register`, `next`, `dispatch`, `serve`)
  built on the pull-style `RequestStream` and `ReplyHandle`;
- a serialisable reference (`InterfaceRef`) and a typed client of it.

```rust
#[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
pub trait Kv {
    /// Read a key.
    #[method(id = 1, schema = 1)]
    async fn get(&self, key: String) -> String;
}

// Generated: KvInterface, KvGet, KvRef, KvClient<P>, KvRequest, KvServer.
let server = KvServer::register(&rpc, AccessClass::Public)?;
let reply = KvClient::bind(&server.interface_ref(), &client)?
    .get()
    .try_get_reply(&"a".to_string())
    .await?;
```

Ids are explicit and never come from names or order. A duplicate method id
is a compile error.

## Usage

Enable the `derive` feature of `moonpool-rpc`. Do not depend on this crate
directly:

```toml
moonpool-rpc = { version = "0.8", features = ["derive"] }
```

Everything the attribute generates can be written by hand with the manual
API. `moonpool-rpc/tests/derive.rs` tests both forms for the same ids, bytes
and behavior. The attribute owns no registry, protocol, delivery or retry
semantics.

## Documentation

- [API documentation](https://docs.rs/moonpool-rpc-derive)
- [moonpool-rpc](https://docs.rs/moonpool-rpc)
- [Admission, Security and Derive](https://pierrez.github.io/moonpool/part4-networking/04-rpc-operations.html) in the Sim Book
