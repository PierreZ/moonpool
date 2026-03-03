# Defining a Service

<!-- toc -->

- Trait definition with `#[service]` attribute
- Method signatures: `async fn method(&self, req: Request) -> Result<Response, RpcError>`
- Generated types: Server (with RequestStream fields), Client (with endpoint accessors), BoundClient (implements the trait)
- Method indexing: 0 reserved for virtual actor dispatch, 1+ for RPC methods
- Code example: Calculator service end-to-end
