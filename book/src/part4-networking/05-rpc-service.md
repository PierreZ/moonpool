# RPC with #[service]

<!-- toc -->

We have peers that manage connections, a wire format that frames messages, and an endpoint map that routes them. But writing the boilerplate for every RPC interface, manually serializing requests, registering endpoints, and correlating responses, gets tedious fast. The `#[service]` proc macro eliminates all of that.

## Define a Trait, Get Everything

Write a Rust trait that describes your service interface, annotate it with `#[service]`, and the macro generates all the networking plumbing.

```rust
#[service]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
}
```

From this single trait definition, the macro generates:

- **`CalculatorServer<C>`** with a `RequestStream` per method, `init()` for dynamic token allocation, `well_known()` for deterministic addressing, and `serve()` for automatic dispatch
- **`CalculatorClient<C>`** with `ServiceEndpoint` fields for each method, constructed via `from_base()` or `well_known()`
- The trait itself, wrapped with `#[async_trait(?Send)]`

## Two Tiers of Endpoint Addressing

Moonpool offers two ways to assign endpoint tokens, matching FoundationDB's dual approach:

### Dynamic (default)

Tokens are allocated at runtime using random UIDs. Each server instance gets a unique base token, so multiple instances of the same service coexist without collision.

```rust
let server = CalculatorServer::init(&transport, JsonCodec);
let base_token = server.base_token(); // random, unique per instance
```

Clients discover the interface via serialization (service registry, out-of-band message, etc.):

```rust
let client = CalculatorClient::from_base(server_addr, base_token, JsonCodec);
```

### Well-known (opt-in)

For system services that need deterministic addressing without discovery, use well-known tokens. Both server and client derive endpoints from the same compile-time constant.

```rust
const WLTOKEN_PING: u32 = 4;

// Server
let server = PingPongServer::well_known(&transport, WLTOKEN_PING, JsonCodec);

// Client (no discovery needed)
let client = PingPongClient::well_known(server_addr, WLTOKEN_PING, JsonCodec);
```

Well-known tokens use `UID::well_known(token_id)` as the base, with method endpoints derived via `base.adjusted(1)`, `.adjusted(2)`, etc.

## What Gets Generated

For a two-method `Calculator` service, the macro produces:

```text
Calculator (trait)
  ├── add(&self, AddRequest) -> Result<AddResponse, RpcError>
  └── sub(&self, SubRequest) -> Result<SubResponse, RpcError>

CalculatorServer<C>
  ├── add: RequestStream<AddRequest, C>    // at base.adjusted(1)
  ├── sub: RequestStream<SubRequest, C>    // at base.adjusted(2)
  ├── init(transport, codec) -> Self       // dynamic tokens
  ├── well_known(transport, token, codec)  // deterministic tokens
  ├── init_at(transport, base, codec)      // explicit base token
  ├── base_token() -> UID                  // for client discovery
  └── serve(transport, handler, providers) -> ServerHandle

CalculatorClient<C>
  ├── from_base(address, base_token, codec) -> Self
  ├── well_known(address, token_id, codec) -> Self
  ├── add: ServiceEndpoint<AddRequest, AddResponse, C>
  └── sub: ServiceEndpoint<SubRequest, SubResponse, C>
```

The `serve()` method is particularly useful: it consumes the server, spawns a background task per method that loops on `recv_with_transport`, and returns a `ServerHandle` that stops everything when dropped.

