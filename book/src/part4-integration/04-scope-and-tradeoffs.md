# What You're Testing (and What You're Not)
<!-- toc -->

Running an axum service inside moonpool-sim exercises a specific slice of your application's behavior. Understanding what falls inside and outside that slice prevents both false confidence and unnecessary skepticism.

## What You Are Testing

**Real handler logic**. Your axum handlers run unchanged. JSON serialization and deserialization happen for real (serde, not mocked). Routing matches against actual paths. Middleware executes in order. Extractors parse real HTTP requests. If your handler has a bug in its JSON response structure, simulation catches it.

**HTTP behavior under chaos**. Requests arrive over simulated TCP with injected latency, connection drops, and data corruption. Hyper's HTTP/1.1 parser processes real wire bytes through a network that actively tries to break things. Half-closed connections, incomplete messages, connection resets mid-response: all exercised.

**Concurrent request handling**. Multiple connections served simultaneously via `spawn_local`. Race conditions between concurrent handlers accessing shared state (your `Store` fake) surface under different scheduling orders across seeds.

**Error paths**. Connection failures, timeouts, process reboots mid-request, store failures via `buggify!()`. The workload validates that your service handles these gracefully: returns appropriate status codes, doesn't panic, doesn't corrupt state.

**Recovery after crash**. With attrition enabled, moonpool kills and restarts processes. Your `Process::run` method executes fresh after each reboot. If your service leaks state across restarts or fails to rebind its listener, simulation finds it.

## What You Are Not Testing

**Production startup code**. `axum::serve()` binds a real `tokio::net::TcpListener` and manages the accept loop internally. In simulation, we use `hyper::server::conn::http1::serve_connection` with a manual accept loop. If your production startup has a bug (wrong bind address, missing middleware registration), simulation won't catch it.

**Real database execution**. The `Store` fake is a BTreeMap, not Postgres. SQL query plans, transaction isolation levels, connection pool behavior, schema migrations: none of these are exercised. A query that works on BTreeMap but generates incorrect SQL won't be caught.

**TLS**. Simulated TCP streams are plaintext. TLS handshake failures, certificate validation, protocol negotiation: not exercised. If your service has a TLS configuration bug, you need integration tests against real TLS.

**Real TCP backpressure and congestion**. moonpool-sim models connection-level faults (drops, latency, corruption) but not TCP flow control, window sizing, or congestion algorithms. If your service has a bug that only manifests under real TCP backpressure, simulation won't find it.

**OS resource limits**. File descriptor exhaustion, memory pressure, CPU scheduling. The simulation runs in a single process with no OS-level resource constraints. A service that leaks file descriptors will appear healthy in simulation.

## The Same Tradeoff Everyone Makes

This is the exact tradeoff every simulation system makes. AWS has run [ShardStore](https://www.amazon.science/publications/using-lightweight-formal-methods-to-validate-a-key-value-storage-node-in-amazon-s3) for over 15 years with the same architecture: real application logic, simulated network, faked storage. FoundationDB simulates network and disk but not the OS kernel. TigerBeetle simulates I/O but not the filesystem. Antithesis runs real binaries but controls the kernel, not the hardware.

The common thread: simulate the **interaction boundaries** where interesting failures occur (network, storage I/O), use real implementations for **computation** (parsers, serializers, business logic), and accept that some classes of bugs require different testing approaches.

## Incremental Adoption

You don't have to simulate your entire application on day one. Start with one module. The trait boundary you create for simulation (`Store`, `Cache`, `MessageBroker`) also improves your production architecture. Dependency injection makes code testable with or without simulation. Trait-based boundaries make components swappable.

Each module you bring under simulation compounds coverage. Your first module finds bugs in its own error handling. Your second module, interacting with the first under chaos, finds bugs in the interaction. By the third module, the simulation is finding bugs you didn't know to look for.

The entry cost is one trait and one fake per dependency. The ongoing cost is maintaining fakes when traits change (the Oxide convention of shipping fake alongside trait helps). The return is deterministic, reproducible, chaos-tested coverage of your actual application logic, running in milliseconds on every commit.
