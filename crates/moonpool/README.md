# moonpool

Deterministic simulation testing for distributed systems in Rust.

`moonpool` is the facade crate. It re-exports `moonpool-core` (provider
traits, `TokioProviders`) and, with the `sim` feature, `moonpool-sim` (the
simulator, chaos, assertions and exploration). Optional integrations live in
the `moonpool::hyper`, `moonpool::prometheus` and `moonpool::rpc` modules.

Moonpool follows [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

> **Note:** This is a hobby-grade project under active development.

## Same code in production and simulation

Code depends on provider traits, never on tokio. Tests run it on the
simulator's providers: logical time, one seeded random stream, a simulated
network and disk with faults. Production runs it on `TokioProviders`. Shipped
code is never wrapped in a simulation variant. It marks fault sites with
`hint!` and `buggify!`, which are inert outside a simulation.

## Example

```rust
use moonpool::prelude::*;
use moonpool::{Chaos, ChaosMode};

let report = SimulationBuilder::new()
    .processes(3, || Box::new(MyNode::default()))
    .workload(MyWorkload::default())
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
    ])
    .set_iterations(100)
    .run()?;
assert_eq!(report.failed_runs, 0);
```

The crate documentation has a complete, compiling version with a `Process`
and a `Workload`.

## Features

| Feature | Default | Adds |
|---------|---------|------|
| `sim` | yes | `moonpool-sim`: simulator, chaos, assertions, exploration |
| `tokio` | yes | `TokioProviders` and the `select!` passthrough |
| `hyper` | no | `moonpool::hyper`: HTTP/2, tonic and axum over the providers |
| `prometheus` | no | `moonpool::prometheus`: a `prometheus` registry in the report |
| `rpc` | no | `moonpool::rpc`: typed dynamic-endpoint RPC |
| `rpc-derive` | no | the `#[moonpool_rpc::service]` attribute |
| `rpc-tls` | no | TLS sessions for `rpc`, server-authenticated or mutual (native only) |
| `rpc-tls-pki` | no | Certificate making for those sessions (`security::tls::pki`, native only) |
| `rpc-jwt` | no | JWT/JWKS request verification for `rpc` (native only) |

## Using in production

Keep the simulator and the explorer out of the release binary:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio"] }
```

## Examples

- `examples/retrying_worker.rs`: one function, run on `TokioProviders` in
  `main` and under the simulator in its `#[test]`.
- `examples/metastable_grpc_retry_storm.rs`: a gRPC retry storm (a
  metastable failure) rendered as an ASCII time series. Needs the `sim`,
  `hyper` and `prometheus` features.

## Documentation

- [The Sim Book](https://pierrez.github.io/moonpool/)
- [API documentation](https://docs.rs/moonpool)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
