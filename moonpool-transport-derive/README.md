# moonpool-transport-derive

Proc-macros for moonpool RPC interfaces.

## `#[service]` — RPC Interface Macro

Generates all boilerplate from a trait definition.

```rust,ignore
#[service]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
}
```

Generates:
- `CalculatorHandler` trait (renamed from `Calculator`) with `#[async_trait]` and `Send + Sync + 'static` supertraits
- `Calculator` struct with `InterfaceMethod` fields for both server and client use

## Documentation

- [API Documentation](https://docs.rs/moonpool-transport-derive)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
