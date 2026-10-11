# moonpool-rpc-sim

The deterministic-simulation harness of `moonpool-rpc`: processes,
workloads, independent oracles and the campaigns that CI runs. This crate is
not published. `moonpool-rpc` never depends on it, nor on `moonpool-sim`,
even through a feature.

Each campaign runs the same `moonpool-rpc` code that runs on real TCP. The
oracles never read the transport: handlers record what they executed in a
ledger, and the workload judges its own outcomes against that ledger.

## Campaigns

| Binary | Module | What it proves |
|---|---|---|
| `sim-rpc-foundations` | `foundations` | Typed request/reply between two process groups and a surviving workload. The corruption suite: it alone draws bit flips |
| `sim-rpc-delivery` | `delivery` | Every delivery mode, lost replies, crashes, simultaneous connects, bootstrap names, failure-monitor transitions |
| `sim-rpc-interfaces` | `interfaces` | Interfaces published, stored, recruited and forwarded across same-address restarts |
| `sim-rpc-balance` | `balance` | Balanced calls under separate retry and duplicate permissions, hedging, cancellation, late losers |
| `sim-rpc-streams` | `streams` | Reply streams with consumption credit, abandonment, saturation, bounded admission, producer reboots |
| `sim-rpc-security` | `security` | Credentials and access under key rotation, UTC transitions, crashes and mixed protocol versions |
| `sim-rpc-qualification` | `qualification` | All of the above at once, with a recovery phase, resource baselines and semantic replay |

Every campaign except foundations masks network bit flips with
`without_corruption()`. Each campaign also records which of
`moonpool-rpc`'s own buggify sites fired (session cuts behind a request,
spurious overload refusals, deferred stream acknowledgements, spurious
failure-monitor wakeups).

## Running

Run the campaigns until coverage is stable, under the determinism canary:

```sh
cargo xtask sim run rpc               # every rpc campaign
cargo xtask sim run rpc-foundations   # one campaign
```

Run the bounded, deterministic seed budgets as tests:

```sh
cargo nextest run -p moonpool-rpc-sim
```

Replay one seed from code:

```rust
let observations = Arc::new(Mutex::new(Vec::new()));
let report = moonpool_rpc_sim::foundations::campaign(WorkloadConfig::campaign(), &observations)
    .set_debug_seeds(vec![42])
    .set_iterations(1)
    .run()?;
```

## Real-network examples

The same RPC code on `TokioProviders` over localhost TCP:

```sh
cargo run -p moonpool-rpc-sim --example tcp_echo
cargo run --release -p moonpool-rpc-sim --example tcp_latency
cargo run --release -p moonpool-rpc-sim --example balance_latency
cargo run --release -p moonpool-rpc-sim --example rpc_soak -- --smoke
```
