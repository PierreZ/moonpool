# Defining a Service

<!-- toc -->

Now that we understand what `#[service]` generates, let us walk through defining a complete service from scratch: the trait definition, the request and response types, and how the generated code fits together.

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

Methods are assigned indices starting at 1 in declaration order. Index 0 is reserved for virtual actor dispatch (used by `&mut self` services).

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

## Actor Mode: &mut self

For stateful actors, use `&mut self` receivers instead. The macro detects this and switches to actor mode:

```rust
#[service(id = 0xBA4E_4B00)]
trait BankAccount {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    async fn withdraw(&mut self, req: WithdrawRequest) -> Result<BalanceResponse, RpcError>;
    async fn get_balance(&mut self, req: GetBalanceRequest) -> Result<BalanceResponse, RpcError>;
}
```

The generated trait methods receive an additional `ctx: &ActorContext<P, C>` parameter that provides access to providers and actor identity:

```rust
#[async_trait::async_trait(?Send)]
impl BankAccount for BankAccountImpl {
    async fn deposit<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        req: DepositRequest,
    ) -> Result<BalanceResponse, RpcError> {
        let new_balance = self.balance() + req.amount;
        // ...
        Ok(BalanceResponse { balance: new_balance })
    }
}
```

The `#[actor_impl(BankAccount)]` attribute on the `ActorHandler` impl then auto-generates the `actor_type()` and `dispatch()` methods, leaving you to provide only the lifecycle overrides you need:

```rust
#[actor_impl(BankAccount)]
impl ActorHandler for BankAccountImpl {
    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateAfterIdle(Duration::from_secs(30))
    }
}
```

If you need no lifecycle customization at all, an empty impl block works:

```rust
#[actor_impl(BankAccount)]
impl ActorHandler for BankAccountImpl {}
```

The macro fills in all the defaults.

## Serialization

The `#[service]` macro is codec-agnostic. The generated server and client are generic over `C: MessageCodec`. In practice, `JsonCodec` is the standard choice, but you can provide any codec that implements the `MessageCodec` trait.

Client types serialize only the base endpoint (address + base UID). Method endpoints are derived at runtime using `UID::new(interface_id, method_index)`, so adding methods does not change the serialized client representation.
