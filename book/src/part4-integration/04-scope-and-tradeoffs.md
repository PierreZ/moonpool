# What You're Testing (and What You're Not)
<!-- toc -->

Running an axum or tonic service inside moonpool-sim exercises a specific
slice of your application. Knowing what sits inside that slice and what sits
outside prevents both false confidence and needless doubt.

## What You Are Testing

**Real handler logic.** Your axum handlers and tonic service methods run
unchanged. serde serializes and deserializes for real. Routing matches real
paths. Middleware runs in order. Extractors parse real requests. protobuf
encodes and decodes real frames. A bug in your JSON response shape or in a
status mapping shows up in simulation.

**HTTP behavior under chaos.** Requests cross a simulated TCP connection with
injected latency, connection drops, clogs and corrupted bytes. hyper's HTTP/1
parser and the h2 frame machinery process real wire bytes through a network
that tries to break them. Half-closed connections, truncated messages, resets
mid-response: all of it runs.

**Concurrent request handling.** The HTTP/1 example drives every open
connection from one `FuturesUnordered`. The h2 example spawns one task per
connection and multiplexes streams inside it. Either way, the seeded executor
interleaves handlers differently on every seed, so races on shared state (your
`Store` fake, your counters) surface.

**Error paths.** Connection failures, timeouts, store failures from
`buggify!`, mid-stream aborts. The workload checks that the service answers
with the right status, does not panic, and does not corrupt state.

**Recovery after a reboot.** With `Chaos::Attrition` enabled, moonpool kills
and restarts processes. `Process::run` starts fresh after each reboot. A
service that leaks state across restarts, or fails to rebind its listener, is
found. With `Chaos::Outage`, a whole process group goes down together.

**Reconnection.** The `ReconnectingChannel` rebuilds its connection after the
server dies, with deterministic backoff. The tonic example asserts
`grpc_channel_recovered` to prove a later round succeeded after an earlier
one lost its connection.

**Time-dependent logic.** Deadlines, keepalive pings, retry backoff and the
retry storm in `metastable_grpc_retry_storm` all run on simulated time. A
40-second experiment takes milliseconds, and the timing is the same on every
replay.

## What You Are Not Testing

**Production startup code.** In production, `axum::serve()` or
`tonic::transport::Server` binds a real listener and owns the accept loop. In
simulation, you write the accept loop and call `serve_connection`. A bug in
the production startup path (wrong bind address, a middleware you forgot to
register) is not caught here. Keep that path small and test it once with a
real socket.

**Real database execution.** The `Store` fake is a `BTreeMap`, not Postgres.
Query plans, isolation levels, connection pools, migrations: none of these run.
A query that works against the fake but generates wrong SQL is not caught.

**TLS.** Simulated streams are plaintext. Handshake failures, certificate
validation and protocol negotiation are not exercised. moonpool-rpc has a
`tls` feature for its own transport, but hyper over `HyperIo` carries no TLS.

**Real TCP congestion.** moonpool-sim models connection-level faults and
end-to-end [flow control](../part3-building/10-network-faults.md#flow-control),
a fixed byte window per direction that a slow reader fills. It does not model
congestion algorithms, window autotuning or retransmission timers. A bug that
only appears under a kernel's congestion behavior is not found. A bug that
appears because a writer never expected to block is.

**OS resource limits.** File descriptor exhaustion, memory pressure, kernel
scheduling. The simulation runs in one process with no OS limits. A service
that leaks file descriptors looks healthy here.

**Wall-clock cost of your code.** Simulated time only advances when the code
sleeps or waits on I/O. A handler that burns 50 ms of CPU costs nothing unless
you say so. `cpu_charge(duration)` from `moonpool-buggify` names that cost,
and [CPU Time and Gray Failures](../part3-building/26-cpu-and-gray-failure.md)
shows how the simulator turns it into latency and slow machines.

**One residual wall-clock read.** hyper's server builder exposes no knob for
the reset-stream expiry, so a server that resets h2 streams keeps a small
dependence on real time. The client side is fully bounded by count. See
[The hyper Stack](./08-hyper-stack.md#why-any-of-this-is-deterministic).

## The Same Tradeoff Everyone Makes

Every simulation system draws this line. AWS's
[ShardStore](https://www.amazon.science/publications/using-lightweight-formal-methods-to-validate-a-key-value-storage-node-in-amazon-s3)
runs real application logic over simulated network and faked storage.
FoundationDB simulates network and disk but not the kernel. TigerBeetle
simulates I/O but not the filesystem. Antithesis runs real binaries and
controls the hypervisor, not the hardware.

The common thread: simulate the **interaction boundaries** where failures
happen (network, storage, time), run real implementations for
**computation** (parsers, serializers, business logic), and accept that some
bug classes need other tests.

## Incremental Adoption

You do not have to simulate the whole application on day one. Start with one
module. The trait boundary you draw for simulation (`Store`, `Cache`,
`MessageBroker`) also improves the production architecture. Dependency
injection makes the code testable with or without simulation.

Each module you bring in compounds coverage. The first module finds bugs in
its own error handling. The second module, talking to the first under chaos,
finds bugs in the interaction. By the third module, the simulation finds bugs
you did not know to look for.

The entry cost is one trait and one fake per dependency. The ongoing cost is
keeping fakes in step with traits, which the Oxide convention of shipping them
together keeps small. The return is deterministic, reproducible, chaos-tested
coverage of your real application logic, in milliseconds, on every commit.
