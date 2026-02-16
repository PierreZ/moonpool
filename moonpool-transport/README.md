# moonpool-transport

FDB-style transport layer for the moonpool simulation framework.

## Why: Same Code, Different Environments

Networking primitives that work identically in simulation and production. Your application code doesn't change—only the provider implementation does.

This follows FoundationDB's FlowTransport patterns, where the same fdbserver code runs in both real deployments and the simulator.

## How: Provider Swapping

The transport layer depends on `NetworkProvider` and `TimeProvider` traits from moonpool-core:

| Environment | NetworkProvider | TimeProvider |
|-------------|-----------------|--------------|
| Production | `TokioNetworkProvider` | `TokioTimeProvider` |
| Simulation | `SimNetworkProvider` | `SimTimeProvider` |

Build your transport once with provider traits. In production, inject tokio-based providers for real TCP. In simulation, inject sim providers that route through the deterministic event loop.

## RPC Semantics

**Request/Response with Correlation**: Every request gets a unique correlation ID. Responses are routed back to the correct waiting caller.

**Automatic Reconnection**: When connections drop, peers reconnect with exponential backoff. Messages queue during disconnection and drain when connectivity returns.

**Message Queuing**: The transport buffers outbound messages during network failures rather than failing immediately. This matches real-world patterns where brief outages shouldn't cause application errors.

## Wire Format

- Length-prefixed packets for framing
- CRC32C checksums for integrity
- Corruption detection triggers reconnection rather than silent failures

## Architecture

```text
┌─────────────────────────────────────────────────┐
│              Application Code                    │
│         Uses NetTransport + RPC                  │
├─────────────────────────────────────────────────┤
│     NetTransport (endpoint routing)             │
│     • Multiplexes connections per endpoint      │
│     • Request/response with correlation         │
├─────────────────────────────────────────────────┤
│     Peer (connection management)                │
│     • Automatic reconnection with backoff       │
│     • Message queuing during disconnection      │
├─────────────────────────────────────────────────┤
│     Wire Format (serialization)                 │
│     • Length-prefixed packets                   │
│     • CRC32C checksums                          │
└─────────────────────────────────────────────────┘
```

## The `#[service]` Macro

The `moonpool-transport-derive` crate (re-exported here) provides a unified macro that auto-generates all boilerplate from a trait definition:

- **`&self` methods** (RPC mode) — generates `Server`, `Client`, `BoundClient`, and `serve()` for automatic request dispatch
- **`&mut self` methods** (Actor mode) — generates `Ref` for typed actor calls, `dispatch_*()` for routing, and a methods module

```rust,ignore
#[service(id = 0xCA1C_0000)]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
}
```

## Documentation

- [API Documentation](https://docs.rs/moonpool-transport)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
