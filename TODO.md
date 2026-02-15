<!--
INSTRUCTIONS FOR CLAUDE CODE (re-read this block every session start and after every compaction):

1. Re-read this file (TODO.md) from disk at session start, after every compaction, and before starting any new task. Never trust compaction summaries for this file.
2. Re-read `docs/references/vertical-paxos.pdf` after compaction or when in doubt about Vertical Paxos II protocol details (ballot activation, Phase 1/2, VFindSafe, master flow).
3. Update this file immediately when completing, starting, or blocking a task.
4. Only mark tasks complete AFTER running:
   - `cargo install cargo-nextest --locked` (if not already installed)
   - `nix develop --command cargo fmt`
   - `nix develop --command cargo clippy`
   - `nix develop --command cargo nextest run`
   All three must pass.
5. When compacting, preserve: current task ID + progress step, modified file paths, failing test names, and key decisions made.
6. Reference tasks by ID (T-XXX) in commit messages.
7. When writing code, add extensive Paxos explanations in rustdoc and comments. Assume reviewer has Raft background but zero Paxos knowledge. Map concepts: ballot ≈ term, acceptor ≈ voter/follower, leader ≈ leader, write quorum ≈ commit quorum, configuration ≈ membership.
8. Work one task at a time. Commit after each.
-->

## Current Focus

> **Task**: _none_
> **Status**: not started
> **Modified files**: _none_

## Up Next

**Foundation**
- [ ] `T-001` 🔴 **Scaffold moonpool-paxos crate** — Cargo.toml (deps: moonpool-transport, moonpool-core, serde, async-trait; dev-deps: moonpool-sim, moonpool-explorer, tokio), lib.rs with crate-level doc explaining VP II vs Raft, add to workspace members
- [ ] `T-002` 🔴 **Define core types** — `BallotNumber(u64)`, `LogSlot(u64)`, `Configuration { ballot, leader: NetworkAddress, acceptors: Vec<NetworkAddress> }`, `PaxosError` enum (StaleBallot, NotLeader, QuorumNotReached, Timeout, Network, Storage)
- [ ] `T-003` 🔴 **Define Command + PaxosStorage traits** — `trait Command: Serialize + DeserializeOwned`, `trait PaxosStorage` (load/store vote, maxBallot, log entries), `InMemoryPaxosStorage` impl
- [ ] `T-004` 🔴 **Define ConfigurationMaster trait** — `trait ConfigurationMaster` (new_ballot, report_complete, wait_activated, current_config), `InMemoryConfigMaster` impl with strict VP II flow

**Acceptor (Paper Section 4.1-4.2, Figure 3)**
- [ ] `T-005` 🟡 **Define AcceptorService** — `#[service]` trait with `prepare(Phase1aRequest) -> Phase1bResponse` and `accept(Phase2aRequest) -> Phase2bResponse`, AcceptorState struct
- [ ] `T-006` 🟡 **Implement Phase1a handler** — On 1a: if msg.bal >= maxBallot, set maxBallot, reply with vote[prevBal]. Else ignore. Explain: "like Raft's RequestVote but for a specific log slot"
- [ ] `T-007` 🟡 **Implement Phase2a handler** — On 2a: if msg.bal >= maxBallot, set vote[bal]=val, reply 2b. Explain: "like Raft's AppendEntries accept, but the value was already chosen by Phase 1"

**Leader / Primary (Paper Section 4.3, Figure 4)**
- [ ] `T-008` 🟡 **Define LeaderService** — `#[service]` trait with `submit(CommandRequest) -> CommandResponse`, LeaderState struct (ballot, prevBal, safeVal, log)
- [ ] `T-009` 🟡 **Implement Phase 1 (VFindSafe)** — Send 1a to prev config acceptors, collect 1b. If any voted value found → safeVal. If read quorum all None → AllSafe. Explain: "like Raft's leader reading uncommitted entries after election, but formalized"
- [ ] `T-010` 🟡 **Implement Phase 2** — Send 2a(bal, val) to ALL acceptors (write quorum = all). Wait for ALL 2b responses. Value is now chosen. Explain: "like Raft replication but requiring ALL followers, not just majority"
- [ ] `T-011` 🟡 **Implement client request path** — Sequential: one slot at a time. Client sends command → leader assigns next slot → Phase 2 → respond. If safeVal != AllSafe, must use that value first.

**Configuration Master (Paper Section 4.3, Figure 5)**
- [ ] `T-012` 🟡 **Implement InMemoryConfigMaster** — Track curBallot, nextBallot. Send newBallot(bal, prevBal=curBallot). On complete(bal, prevBal): if prevBal==curBallot, activate and send activated(bal).
- [ ] `T-013` 🟡 **Implement strict VP II activation** — New config starts inactive. Leader does Phase1 + Phase2 (if needed) → sends complete → master activates → leader receives activated → begins serving. Inactive ballots have no effect.

**Reconfiguration**
- [ ] `T-014` 🟡 **Implement failure detector** — Primary sends periodic heartbeats to master via RPC. Master tracks last_heartbeat per node. On timeout: trigger reconfiguration. Use TimeProvider for sim-friendly clocks.
- [ ] `T-015` 🔴 **Implement reconfiguration flow** — Master picks new leader from surviving acceptors (read quorum = 1, so new leader already has prev state). New leader: Phase1 from prev config → catch up → complete → activated → serve. Explain: "the key VP II insight: new leader IS a read quorum, so state transfer is local"

**Catch-up & Leases**
- [ ] `T-016` 🟡 **Implement CatchUp RPC** — Separate `catch_up(from_slot, to_slot) -> Vec<LogEntry>` endpoint. Lagging backup requests missing entries from primary. Not part of Paxos protocol proper, but needed for practical recovery.
- [ ] `T-017` 🟡 **Implement conservative leases** — Primary holds time-bounded lease. Reads served locally during lease. Master waits lease_duration + max_clock_drift before electing new primary. Lease refreshed via heartbeat ACK.

**Client**
- [ ] `T-018` 🟡 **Implement PaxosClient** — Discovers primary from master. Submits commands. On NotLeader error: re-discover primary and retry. Explain: "like Raft client redirection but discovery goes through the master, not the cluster"

**Simulation Tests**
- [ ] `T-019` 🔴 **Scaffold sim test** — 5-node topology (3 acceptors + 1 master + 1 client). SimulationBuilder with enable_exploration. Basic workload: client submits N commands, verify all committed.
- [ ] `T-020` 🔴 **Safety assertions** — `assert_always!`: no two different values chosen for same slot, no split-brain (at most one active primary per ballot), acceptor never votes in ballot < maxBallot, write quorum = all ACK before chosen
- [ ] `T-021` 🔴 **Liveness assertions** — `assert_sometimes!`: consensus reached for at least one slot, reconfiguration completes after primary failure, lease expires and new primary elected, catch-up completes for lagging backup
- [ ] `T-022` 🟡 **Exploration assertions** — `assert_sometimes_each!`: reconfig depth (successive failovers), log slot depth (commands committed). `assert_sometimes_all!`: all backups caught up simultaneously, full write quorum healthy + consensus in progress. Adaptive config (batch=20, min=100, max=200, per_mark=1K)

## Blocked / Waiting

_none_

## Completed

<!-- Format: - [x] `T-XXX` **Title** — Completed YYYY-MM-DD, commit: abc1234 -->
