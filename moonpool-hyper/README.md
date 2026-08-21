# moonpool-hyper

hyper 1.x integration for the moonpool provider traits.

## Why

hyper is runtime-agnostic in principle, but every ready-made adapter
(`hyper-util`'s `TokioExecutor`, `TokioTimer`, `TokioIo`) hard-codes tokio.
Running a hyper stack (tonic gRPC, axum, plain HTTP) inside moonpool's
deterministic simulation means giving hyper the same hooks over
`TaskProvider`, `TimeProvider` and the streams a `NetworkProvider` hands out.

## What

| Type | Role |
|------|------|
| `HyperExecutor<T>` | `hyper::rt::Executor` over a `TaskProvider` |
| `HyperTimer<T>` | `hyper::rt::Timer` over a `TimeProvider` |
| `HyperIo<S>` | `hyper::rt::Read` + `hyper::rt::Write` over a futures-io stream |
| `TowerToHyperService<S>` | tower service to hyper service adapter |
| `KeepAlive` | h2 PING keepalive settings shared by client and server |

`HyperIo` replaces the `tokio_util::compat::Compat` + `hyper_util::rt::TokioIo`
two-hop bridge: every moonpool network stream already implements the futures-io
`AsyncRead`/`AsyncWrite` pair, which is all hyper's IO traits need.

The same code runs in production (tokio providers, real TCP) and in simulation
(sim providers, logical time), which is the whole point of the provider
pattern.

## Documentation

- [API Documentation](https://docs.rs/moonpool-hyper)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
