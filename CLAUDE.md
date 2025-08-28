# Claude Context for Moonpool

## Project Overview
Moonpool is a Rust workspace for building distributed systems tooling. Current focus: deterministic simulation framework in `moonpool-simulation/`.

## Development Environment
**CRITICAL**: Must use Nix development shell:
```bash
nix develop --command <cargo-command>
```

## Phase Completion Criteria
Phase complete only when ALL pass:
```bash
nix develop --command cargo fmt
nix develop --command cargo clippy  
nix develop --command cargo nextest run
```

## Architecture Constraints
- **Single-core execution**: Deterministic behavior, no Send/Sync complexity
- **No unwrap()**: Use `Result<T, E>` with `?` operator (enforced by clippy)
- **Documentation required**: All public items must be documented
- **Networking traits**: Use `#[async_trait(?Send)]`
- **Use traits, not implementations**: Always use trait definitions, never concrete implementation structs
- **KISS principle**: Keep It Simple, Stupid - prefer simple solutions over complex ones

## Simulation Testing Philosophy
**GOAL**: Add sometimes assertions to critical code paths and reach 100% of them through simulation chaos testing.

**CRITICAL**: Simulation finds bugs by testing worst-case scenarios.

**When failing seeds found:**
1. Debug manually with detailed logging
2. Identify root cause in production code
3. Fix underlying bug
4. Verify fix with failing seed
5. Re-enable chaos testing

**Never ignore failing seeds - they represent real production bugs.**

## Implementation Status

**Core Infrastructure**: Event queue with deterministic ordering, logical time advancement, SimWorld coordination harness with handle pattern, comprehensive error handling.

**Network Abstraction**: NetworkProvider trait system enabling seamless swapping between real (TokioNetworkProvider) and simulated (SimNetworkProvider) networking implementations.

**Testing Framework**: Thread-local RNG with deterministic seeding, assertion macros (`always_assert!`/`sometimes_assert!`) with statistical tracking, SimulationReport system with multi-iteration testing and comprehensive metrics.

**Fault Tolerance**: Resilient Peer implementation with automatic reconnection, exponential backoff, message queuing during disconnections, and robust handling of network failures across all provider types.

## Sometimes Assertions (Antithesis-Style)
**Purpose**: Verify rare but critical code paths are tested.

**Always Assertions**: Guard invariants that must NEVER fail
**Sometimes Assertions**: Verify error/recovery paths are exercised when multiple seeds are runned

**Strategic placement**:
- Error handling blocks
- Reconnection/retry logic  
- Queue overflow scenarios
- Timeout handlers
- Resource exhaustion paths

**Anti-patterns**: Don't use on normal execution paths or without clear purpose.

## Reference Code Mapping

When working on Rust implementation, read corresponding reference sections:

- **Peer** → FoundationDB Peer class (docs/references/fdb/FlowTransport.h:147-191, docs/references/fdb/FlowTransport.actor.cpp:1016-1125)
- **SimWorld** → FoundationDB Sim2 class (docs/references/fdb/sim2.actor.cpp:1051+)  
- **Configuration design** → TigerBeetle PacketSimulator (docs/references/tigerbeetle/packet_simulator.zig:12-488)
- **Connection management** → FoundationDB connectionKeeper (docs/references/fdb/FlowTransport.actor.cpp:760-900)
- **Exponential backoff** → FoundationDB reconnection logic (docs/references/fdb/FlowTransport.actor.cpp:892-897)
- **Message reliability** → FoundationDB ReliablePacket system (docs/references/fdb/Net2Packet.h:30-111)
- **Request-response pattern** → FoundationDB ping workload (docs/references/fdb/Ping.actor.cpp:29-38, 147-169)
- **Message queuing** → FoundationDB UnsentPacketQueue (docs/references/fdb/Net2Packet.h:43-91)

**Note**: We focus on TCP-level simulation (connection cutting/clogging) rather than packet-level faults, since TCP abstracts packet reliability for applications.