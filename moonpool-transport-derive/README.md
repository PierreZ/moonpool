# moonpool-transport-derive

Proc-macros for moonpool RPC interfaces and virtual actors.

## `#[service]` — Unified Interface Macro

A single attribute macro that generates all boilerplate from a trait definition. The mode is auto-detected from method receivers:

### RPC Mode (`&self` methods)

```rust,ignore
#[service(id = 0xCA1C_0000)]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
}
```

Generates:
- `CalculatorServer<C>` with `RequestStream` fields, `init()`, and `serve()`
- `CalculatorClient` with endpoint accessors and `bind()` method
- `BoundCalculatorClient<P, C>` implementing the `Calculator` trait

### Actor Mode (`&mut self` methods)

```rust,ignore
#[service(id = 0xBA4E_4B00)]
trait BankAccount {
    async fn deposit(&mut self, req: DepositRequest) -> Result<BalanceResponse, RpcError>;
    async fn balance(&mut self, req: BalanceRequest) -> Result<BalanceResponse, RpcError>;
}
```

Generates:
- `BankAccountRef<P>` for typed actor calls
- `dispatch_bank_account()` function for method routing
- `bank_account_methods` module with `ACTOR_TYPE` and method constants

## `#[actor_impl]` — ActorHandler Boilerplate

Applied to `impl ActorHandler for T` blocks. Auto-generates `actor_type()`, `dispatch()`, and default lifecycle hooks (`on_activate`, `on_deactivate`, `deactivation_hint`) if not provided.

```rust,ignore
#[actor_impl(BankAccount)]
impl ActorHandler for BankAccountActor {
    // Only implement domain logic — boilerplate is generated
}
```

## Documentation

- [API Documentation](https://docs.rs/moonpool-transport-derive)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
