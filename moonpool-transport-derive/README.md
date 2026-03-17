# moonpool-transport-derive

Proc-macros for moonpool RPC interfaces.

## `#[service]` — RPC Interface Macro

Generates all boilerplate from a trait definition.

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

## Documentation

- [API Documentation](https://docs.rs/moonpool-transport-derive)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
