# Defining a Service

<!-- toc -->

The `#[service]` macro generates a complete RPC infrastructure from a trait definition. This chapter walks through defining a service from scratch: the trait definition, the request and response types, and how the generated code fits together.

## The Trait Definition

A service starts as a Rust trait with `#[service(id = ...)]`:

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

#[service(id = 0xCA1C_0000)]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn multiply(&self, req: MulRequest) -> Result<MulResponse, RpcError>;
}
```

Every method must follow the same signature pattern: `async fn name(&self, req: RequestType) -> Result<ResponseType, RpcError>`. The macro parses the `Result<T, RpcError>` return type to extract the response type for code generation.

Request and response types need `Serialize` and `Deserialize` derives because they travel over the wire. They also need to be `'static` and `DeserializeOwned`, which standard derives give you automatically.

## Method Indexing

Methods are assigned indices starting at 1 in declaration order. Index 0 is reserved.

For our Calculator:
- `add` gets index 1, routed to `UID(0xCA1C_0000, 1)`
- `multiply` gets index 2, routed to `UID(0xCA1C_0000, 2)`

These indices are stable as long as you do not reorder methods. Adding new methods at the end is safe. Reordering or removing methods changes the wire protocol and breaks compatibility with existing clients.

## Implementing the Server

To handle requests, implement the generated trait on a concrete type:

```rust
struct CalculatorImpl;

#[async_trait::async_trait(?Send)]
impl Calculator for CalculatorImpl {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError> {
        Ok(AddResponse { result: req.a + req.b })
    }

    async fn multiply(&self, req: MulRequest) -> Result<MulResponse, RpcError> {
        Ok(MulResponse { result: req.a * req.b })
    }
}
```

Note the `#[async_trait(?Send)]` attribute. All moonpool services are single-threaded (`!Send`), matching our deterministic execution model. The macro adds this attribute to the generated trait automatically, and you must repeat it on the `impl` block.

## Serialization

The `#[service]` macro is codec-agnostic. The generated server and client are generic over `C: MessageCodec`. In practice, `JsonCodec` is the standard choice, but you can provide any codec that implements the `MessageCodec` trait.

Client types serialize only the base endpoint (address + base UID). Method endpoints are derived at runtime using `UID::new(interface_id, method_index)`, so adding methods does not change the serialized client representation.
