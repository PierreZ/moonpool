# Defining a Service

<!-- toc -->

The `#[service]` macro generates a complete RPC infrastructure from a trait definition. This chapter walks through defining a service from scratch: the trait definition, the request and response types, and how the generated code fits together.

## The Trait Definition

A service starts as a Rust trait with `#[service]`:

```rust
use moonpool::{service, RpcError};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddRequest { a: i32, b: i32 }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddResponse { result: i32 }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MulRequest { a: i32, b: i32 }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MulResponse { result: i32 }

#[service]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn multiply(&self, req: MulRequest) -> Result<MulResponse, RpcError>;
}
```

Every method must follow the same signature pattern: `async fn name(&self, req: RequestType) -> Result<ResponseType, RpcError>`. The macro parses the `Result<T, RpcError>` return type to extract the response type for code generation.

Request and response types need `Serialize` and `Deserialize` derives because they travel over the wire. They also need to be `'static` and `DeserializeOwned`, which standard derives give you automatically.

## Method Indexing

Methods are assigned indices starting at 1 in declaration order. Index 0 is the base token identity.

For our Calculator with a base token `B`:
- `add` gets index 1, routed to `B.adjusted(1)`
- `multiply` gets index 2, routed to `B.adjusted(2)`

These indices are stable as long as you do not reorder methods. Adding new methods at the end is safe. Reordering or removing methods changes the wire protocol and breaks compatibility with existing clients.

## Implementing the Server

To handle requests, implement the generated trait on a concrete type:

```rust
struct CalculatorImpl;

#[async_trait::async_trait]
impl Calculator for CalculatorImpl {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError> {
        Ok(AddResponse { result: req.a + req.b })
    }

    async fn multiply(&self, req: MulRequest) -> Result<MulResponse, RpcError> {
        Ok(MulResponse { result: req.a * req.b })
    }
}
```

Note the `#[async_trait]` attribute on the `impl` block. Service handler traits are dyn-stored, so the macro emits `#[async_trait]` with `Send + Sync + 'static` supertraits on the generated trait. The simulation runtime still uses a single OS thread via `new_current_thread().build()`, but our handlers are **Send-bounded** so customer code can hold `Arc<RwLock<…>>`, `DashMap`, or any other `Send + Sync` state naturally. You re-apply `#[async_trait]` (without `?Send`) on the `impl`, and the compiler enforces that your handler state stays `Send + Sync + 'static`.

## Serialization

The `#[service]` macro is codec-agnostic. The codec is a transport-level concern, set once when building the transport via `NetTransportBuilder::new(providers).codec(JsonCodec)`. The default is `JsonCodec`, so most code never mentions a codec at all. The generated server and client types carry no codec generic.

The generated `Calculator` struct serializes cleanly because each method endpoint stores just the destination address and method UID. Adding methods at the end does not change the serialized representation of existing endpoints.
