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

- **`CalculatorHandler`** trait (renamed from `Calculator`) with `#[async_trait(?Send)]`
- **`LocalCalculator`** struct (server) with `LocalMethod` fields, used to `recv()` requests and `serve()` handlers
- **`Calculator`** struct (client) with `RemoteMethod` fields, used to `get_reply()`, `send()`, and friends

Splitting server and client into two concrete types means mode mismatches are caught at compile time. Server constructors live on `LocalCalculator`; client constructors live on `Calculator`.

## Two Tiers of Endpoint Addressing

Moonpool offers two ways to assign endpoint tokens, matching FoundationDB's dual approach:

### Dynamic (default)

Tokens are allocated at runtime using random UIDs. Each server instance gets a unique base token, so multiple instances of the same service coexist without collision.

```rust
let server = LocalCalculator::init(&transport);
let base_token = server.base_token(); // random, unique per instance
```

Clients discover the interface via serialization (service registry, out-of-band message, etc.):

```rust
let client = Calculator::from_base(server_addr, base_token, &transport);
```

### Well-known (opt-in)

For system services that need deterministic addressing without discovery, use well-known tokens. Both server and client derive endpoints from the same compile-time constant.

```rust
const WLTOKEN_PING: u32 = 4;

// Server (local mode)
let server = LocalPingPong::well_known(&transport, WLTOKEN_PING);

// Client (remote mode, no discovery needed)
let client = PingPong::client_well_known(server_addr, WLTOKEN_PING, &transport);
```

Well-known tokens use `UID::well_known(token_id)` as the base, with method endpoints derived via `base.adjusted(1)`, `.adjusted(2)`, etc.

## What Gets Generated

For a two-method `Calculator` service, the macro produces:

```text
CalculatorHandler (trait, renamed from Calculator)
  ├── add(&self, AddRequest) -> Result<AddResponse, RpcError>
  └── sub(&self, SubRequest) -> Result<SubResponse, RpcError>

LocalCalculator (server struct)
  ├── add: LocalMethod<AddRequest, AddResponse>   // at base.adjusted(1)
  ├── sub: LocalMethod<SubRequest, SubResponse>   // at base.adjusted(2)
  ├── init(transport) -> Self               // dynamic tokens
  ├── well_known(transport, token) -> Self   // deterministic tokens
  ├── init_at(transport, base) -> Self       // explicit base token
  ├── base_token() -> UID                   // for client discovery
  └── serve(handler, providers) -> ServerHandle

Calculator (client struct)
  ├── add: RemoteMethod<AddRequest, AddResponse>
  ├── sub: RemoteMethod<SubRequest, SubResponse>
  ├── from_base(addr, base, transport)        // discovered token
  ├── client_well_known(addr, token, transport) // deterministic
  ├── deserialize_with(transport, deserializer) // bind to local transport
  └── base_token() -> UID
```

The `serve()` method is particularly useful: it consumes the local interface, spawns a background task per method that loops on `recv()`, and returns a `ServerHandle` that stops everything when dropped. The transport is bound at construction, so `serve()` only needs the handler and providers.

