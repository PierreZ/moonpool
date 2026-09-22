# FoundationDB reference sources

Verbatim copies of FoundationDB source files (Apache 2.0, © Apple Inc. and the
FoundationDB project authors), kept here so moonpool's analyses and plans can cite
exact code. Two snapshots live side by side; **never combine behaviour across them**.

## Provenance

- Repository: <https://github.com/apple/foundationdb>
- Refreshed snapshot: commit `c0c44752df676e4a2d532b5cdb4bf96728a30b78`
  (committed 2026-08-22), copied 2026-09-22. This is the SHA the
  `docs/plans/moonpool-rpc/` plan cites.
- Older snapshot: the simulation/storage files below predate that commit (they were
  copied earlier from a 2024-era tree and carry `.actor` names). They back moonpool's
  existing simulator mapping (`AGENTS.md`, `docs/analysis/foundationdb/simulation.md`) and
  were deliberately left as they were.

At `c0c44752` many files dropped the `.actor` suffix (C++20 coroutines replaced the actor
compiler). Refreshed files use their current upstream names; old names were renamed in git.

## Refreshed at `c0c44752` (RPC layer)

| Local file | Upstream path | Note |
|---|---|---|
| `FlowTransport.cpp` | `fdbrpc/FlowTransport.cpp` | was `FlowTransport.actor.cpp` |
| `FlowTransport.h` | `fdbrpc/include/fdbrpc/FlowTransport.h` | |
| `fdbrpc.h` | `fdbrpc/include/fdbrpc/fdbrpc.h` | |
| `genericactors.h` | `fdbrpc/include/fdbrpc/genericactors.h` | was `genericactors.actor.h` (fdbrpc's, not flow's) |
| `networksender.h` | `fdbrpc/include/fdbrpc/networksender.h` | was `networksender.actor.h` |
| `FailureMonitor.cpp` | `fdbrpc/FailureMonitor.cpp` | was `FailureMonitor.actor.cpp` |
| `FailureMonitor.h` | `fdbrpc/include/fdbrpc/FailureMonitor.h` | |
| `HealthMonitor.cpp` | `fdbrpc/HealthMonitor.cpp` | was `HealthMonitor.actor.cpp` |
| `HealthMonitor.h` | `fdbrpc/HealthMonitor.h` | |
| `LoadBalance.actor.h` | `fdbrpc/include/fdbrpc/LoadBalance.actor.h` | still an actor header upstream |
| `LoadBalance.h` | `fdbrpc/include/fdbrpc/LoadBalance.h` | |
| `MultiInterface.h` | `fdbrpc/include/fdbrpc/MultiInterface.h` | |
| `QueueModel.h` | `fdbrpc/include/fdbrpc/QueueModel.h` | |
| `WellKnownEndpoints.h` | `fdbrpc/include/fdbrpc/WellKnownEndpoints.h` | |
| `Net2Packet.cpp` | `flow/Net2Packet.cpp` | |
| `Net2Packet.h` | `flow/include/flow/Net2Packet.h` | |
| `error_definitions.h` | `flow/include/flow/error_definitions.h` | |

## Added at `c0c44752` (cited by the moonpool-rpc plan)

| Local file | Upstream path |
|---|---|
| `FlowTests.actor.cpp` | `fdbrpc/FlowTests.actor.cpp` |
| `QueueModel.cpp` | `fdbrpc/QueueModel.cpp` |
| `LoadBalance.cpp` | `fdbrpc/LoadBalance.cpp` |
| `Locality.cpp` | `fdbrpc/Locality.cpp` |
| `Locality.h` | `fdbrpc/include/fdbrpc/Locality.h` |
| `IPAllowList.cpp` | `fdbrpc/IPAllowList.cpp` |
| `IPAllowList.h` | `fdbrpc/include/fdbrpc/IPAllowList.h` |
| `JsonWebKeySet.cpp` | `fdbrpc/JsonWebKeySet.cpp` |
| `JsonWebKeySet.h` | `fdbrpc/JsonWebKeySet.h` |
| `TLSConfig.cpp` | `flow/TLSConfig.cpp` |
| `TLSConfig.h` | `flow/include/flow/TLSConfig.h` |
| `tests/AuthzTlsTest.cpp` | `fdbrpc/tests/AuthzTlsTest.cpp` |

The plan also cites `fdbserver/…` and `fdbclient/…` files (worker, cluster controller,
storage server interface, `NativeAPI.actor.cpp`); those are not copied here.

## Older snapshot kept for the simulation mapping

Not refreshed; upstream has since renamed or rewritten most of them (e.g. `fdbrpc/sim2.cpp`,
`flow/Net2.cpp`), so their line citations in `AGENTS.md` and the analyses refer to these
local copies, not to `c0c44752`.

| Local file | Upstream path (at the time of copy) |
|---|---|
| `sim2.actor.cpp` | `fdbrpc/sim2.actor.cpp` |
| `sim2-file.actor.cpp` | excerpt of `fdbrpc/sim2.actor.cpp` (file simulation; no upstream file of this name) |
| `AsyncFileChaos.h` | `fdbrpc/AsyncFileChaos.h` |
| `AsyncFileNonDurable.actor.h` | `fdbrpc/include/fdbrpc/AsyncFileNonDurable.actor.h` (excerpt) |
| `IAsyncFile.h` | `flow/include/flow/IAsyncFile.h` |
| `Net2.actor.cpp` | `flow/Net2.actor.cpp` |
| `Buggify.h` | `flow/include/flow/Buggify.h` |
| `ChaosMetrics.h` | `flow/include/flow/ChaosMetrics.h` |
| `SimulatorMachineInfo.h` | `fdbrpc/include/fdbrpc/SimulatorMachineInfo.h` |
| `Ping.actor.cpp` | `fdbserver/workloads/Ping.actor.cpp` |
| `lost-response.md` | moonpool-authored note; its `genericactors.actor.h` / `fdbrpc.h` / `LoadBalance.actor.h` line citations refer to the older snapshot |
