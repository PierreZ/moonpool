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
> **Status**: T-001 through T-022 complete
> **Modified files**: moonpool-paxos/src/{lib,types,storage,master,acceptor,leader,heartbeat,reconfig,catchup,lease,client}.rs, moonpool-paxos/tests/simulation/{mod,workloads,invariants,test_scenarios}.rs, moonpool-paxos/Cargo.toml, Cargo.toml

## Up Next

_All planned tasks complete._

## Blocked / Waiting

_none_

## Completed

<!-- Format: - [x] `T-XXX` **Title** — Completed YYYY-MM-DD, commit: abc1234 -->
- [x] `T-001` **Scaffold moonpool-paxos crate** — Completed 2026-02-15
- [x] `T-002` **Define core types** — Completed 2026-02-15
- [x] `T-003` **Define Command + PaxosStorage traits** — Completed 2026-02-15
- [x] `T-004` **Define ConfigurationMaster trait** — Completed 2026-02-15
- [x] `T-005` **Define AcceptorService** — Completed 2026-02-15
- [x] `T-006` **Implement Phase1a handler** — Completed 2026-02-15
- [x] `T-007` **Implement Phase2a handler** — Completed 2026-02-15
- [x] `T-008` **Define LeaderService** — Completed 2026-02-15
- [x] `T-009` **Implement Phase 1 (VFindSafe)** — Completed 2026-02-15
- [x] `T-010` **Implement Phase 2** — Completed 2026-02-15
- [x] `T-011` **Implement client request path** — Completed 2026-02-15
- [x] `T-012` **Implement InMemoryConfigMaster** — Completed 2026-02-15
- [x] `T-013` **Implement strict VP II activation** — Completed 2026-02-15
- [x] `T-014` **Implement failure detector** — Completed 2026-02-15
- [x] `T-015` **Implement reconfiguration flow** — Completed 2026-02-15
- [x] `T-016` **Implement CatchUp RPC** — Completed 2026-02-15
- [x] `T-017` **Implement conservative leases** — Completed 2026-02-15
- [x] `T-018` **Implement PaxosClient** — Completed 2026-02-15
- [x] `T-019` **Scaffold sim test** — Completed 2026-02-15
- [x] `T-020` **Safety assertions** — Completed 2026-02-15
- [x] `T-021` **Liveness assertions** — Completed 2026-02-15
- [x] `T-022` **Exploration assertions** — Completed 2026-02-15
