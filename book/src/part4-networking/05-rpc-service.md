# RPC with #[service]

<!-- toc -->

We have peers that manage connections, a wire format that frames messages, and an endpoint map that routes them. But writing the boilerplate for every RPC interface, manually serializing requests, registering endpoints, and correlating responses, gets tedious fast. The `#[service]` proc macro eliminates all of that.

## Define a Trait, Get Everything

The idea is simple: write a Rust trait that describes your service interface, annotate it with `#[service(id = ...)]`, and the macro generates all the networking plumbing.

```rust
#[service(id = 0xCA1C_0000)]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
}
```

From this single trait definition, the macro generates:

- **`CalculatorServer<C>`** with a `RequestStream` per method and an `init()` method that registers all endpoints with the transport
- **`CalculatorClient`** with endpoint accessors and a `bind()` method
- **`BoundCalculatorClient<P, C>`** that implements the `Calculator` trait, so you can call `client.add(req).await?` directly
- The trait itself, wrapped with `#[async_trait(?Send)]`

## Two Modes, One Macro

The macro auto-detects which mode to use based on the method receivers:

**`&self` methods produce RPC mode.** The generated server has `RequestStream` fields for each method. The generated client sends requests over the network. This is for stateless request-response services.

**`&mut self` methods produce actor mode.** The generated code includes an `ActorRef` type for sending typed messages to virtual actors, a `dispatch` function for routing by method discriminant, and a constants module. This is for stateful actors routed by identity.

Mixing `&self` and `&mut self` in the same trait is a compile error. Pick one mode per service.

## The Service ID

Every service needs a unique `id` attribute:

```rust
#[service(id = 0xBA4E_4B00)]
```

This `u64` value becomes the base for all endpoint tokens in the service. Method endpoints are derived using `UID::new(interface_id, method_index)`, where method indices start at 1 (index 0 is reserved for virtual actor dispatch).

The hex convention makes it easy to identify services in wire captures and logs. `0xCA1C` looks like "CALC", `0xBA4E_4B00` looks like "BANKB00". Choose values that are memorable and unique within your system.

## What Gets Generated (RPC Mode)

For a two-method `Calculator` service, the macro produces roughly this structure:

```text
Calculator (trait)
  ├── add(&self, AddRequest) -> Result<AddResponse, RpcError>
  └── sub(&self, SubRequest) -> Result<SubResponse, RpcError>

CalculatorServer<C>
  ├── add: RequestStream<AddRequest, C>    // endpoint at UID(0xCA1C_0000, 1)
  ├── sub: RequestStream<SubRequest, C>    // endpoint at UID(0xCA1C_0000, 2)
  ├── init(transport, codec) -> Self
  └── serve(transport, handler, providers) -> ServerHandle

CalculatorClient
  ├── new(address) -> Self
  ├── bind(transport, codec) -> BoundCalculatorClient
  └── add_endpoint() / sub_endpoint()

BoundCalculatorClient<P, C>
  ├── add(req) -> Result<AddResponse, RpcError>  // sends via transport
  └── sub(req) -> Result<SubResponse, RpcError>
```

The `serve()` method is particularly useful: it consumes the server, spawns a background task per method that loops on `recv_with_transport`, and returns a `ServerHandle` that stops everything when dropped.

## What Gets Generated (Actor Mode)

For a `BankAccount` service with `&mut self` methods:

```text
BankAccount (trait)
  ├── deposit(&mut self, ctx, req) -> Result<BalanceResponse, RpcError>
  └── withdraw(&mut self, ctx, req) -> Result<BalanceResponse, RpcError>

bank_account_methods (module)
  ├── ACTOR_TYPE: ActorType(0xBA4E_4B00)
  ├── DEPOSIT: u32 = 1
  └── WITHDRAW: u32 = 2

BankAccountRef<P, C>
  ├── new(identity, router) -> Self
  ├── deposit(req) -> Result<BalanceResponse, RpcError>
  └── withdraw(req) -> Result<BalanceResponse, RpcError>

dispatch_bank_account(actor, ctx, method, body) -> Result<Vec<u8>, ActorError>
```

The `ActorRef` is a lightweight handle that does not activate the actor on creation. The actor activates on first message, following the Orleans pattern. We will explore actors in detail in Part 5.
