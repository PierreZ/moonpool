# A Brief History

<!-- toc -->

Simulation-driven development did not appear from nowhere. It grew over fifteen years across three eras. Each era widened who could use it and how.

## FoundationDB's bet (2009-2015)

In 2009 a small team set out to build a distributed transactional database. Before they wrote the database, they spent about two years on a simulator. No storage engine, no query layer, no client protocol. Just the simulation.

The thesis was simple and, to most observers, insane. Distributed systems fail in ways that depend on the order of concurrent events. The only way to test those orders systematically is to control them completely.

They built [Flow](https://apple.github.io/foundationdb/flow.html), a C++ extension that compiles actor-style code to single-threaded callbacks. Every connection, disk operation and timer goes through one interface, `INetwork`, with two implementations: `Net2` for production (real TCP over Boost.ASIO) and `Sim2` for simulation (in-memory buffers with deterministic delays). One seeded random number generator controls everything. Same seed, same execution, every time.

Two techniques made the simulation unusually effective. **BUGGIFY** marks points in the code where a rare but legal behavior can be forced: a packet sent out of order, a truncated disk write, a skipped timeout, a shrunk buffer. Each site is activated at random once per run and then fires with a configurable probability. BUGGIFY widens the contract under test far beyond what any specification documents. It tests not what the system is supposed to handle but what it **can** handle.

**Correlated failures** came from a Hurst-exponent model. In a real datacenter, a failing drive makes its rack neighbors more likely to fail. Independent-failure models make cascades astronomically rare. FoundationDB turned the correlation up so that failure patterns a datacenter sees once in years appear in simulation in milliseconds.

The scale was large. [Tens of thousands of simulations every night](https://apple.github.io/foundationdb/testing.html), each simulating minutes to hours of cluster time under heavy fault injection. A physical cluster called **Sinkhole**, real motherboards wired to programmable power switches that toggled continuously, served as the reality check. Sinkhole never found a database bug that the simulator had missed. It found bugs in other software and in hardware.

The cost was equally large: a custom language, a simulation-first architecture and two years of infrastructure before the product. For more than a decade FoundationDB's simulation was a benchmark others admired but could not copy.

## Antithesis generalizes it (2020 to now)

Will Wilson, one of the original FoundationDB engineers, co-founded [Antithesis](https://antithesis.com) around a different question: can the same techniques apply to **any** software, without a rewrite?

The answer was a deterministic hypervisor. Instead of building simulation into the system from day one, Antithesis intercepts nondeterminism at the virtual machine. Any program in any language becomes a pure function from an input byte stream to a sequence of states. Save, restore, fork, replay. No Flow. No custom language. No architectural prerequisite.

On top of the hypervisor sits a guided exploration engine. The [SDK](https://antithesis.com/docs/using_antithesis/sdk/) provides declarative assertions that say **what** to explore without saying **how**: `Sometimes` (reach this condition at least once), `Always` (this invariant holds), `SometimesAll` (drive the search across a frontier of sub-goals). The platform combines these signals with coverage-guided forking and adaptive search.

The results validated the approach. Customers with mature, well-tested systems found new bugs within two to three weeks of onboarding: deep concurrency issues, subtle data corruption, recovery defects that survived years of conventional testing. The engine needs little domain knowledge. In one demonstration Antithesis [beat the game Gradius](https://antithesis.com/blog/gradius/) by watching three bytes of memory and maximizing time alive.

## The adoption wave (2020 to now)

The third era is happening now. Simulation-driven development is spreading beyond the teams that invented it.

[TigerBeetle](https://tigerbeetle.com/), a financial transactions database, adopted deterministic simulation from day one. Its simulator adds storage faults drawn from real hardware failure modes: misdirected reads and writes, phantom writes, torn writes, read corruption. [Dropbox used simulation](https://dropbox.tech/infrastructure/-testing-our-new-sync-engine) to validate its sync engine rewrite. [WarpStream](https://www.warpstream.com/blog/deterministic-simulation-testing-for-our-entire-saas) applied deterministic simulation to its whole SaaS platform. In Rust, the `madsim` crate showed that a tokio-compatible deterministic runtime could drive existing async code.

[Clever Cloud](https://www.clever-cloud.com/) uses simulation-driven development to build Materia, a distributed multi-model multi-tenant database. Thirty minutes of simulation covers roughly 24 hours of equivalent chaos testing.

What took an elite team two years of custom infrastructure in 2009 is becoming available to small teams on ordinary languages and frameworks. The investment is shrinking. The bugs found per engineering hour are growing.

## Where moonpool draws from

Moonpool takes one idea from each era, and the rest of the book names the source whenever it matters.

From **FoundationDB**: the single-threaded simulator with one random stream, `INetwork` as the provider pattern, BUGGIFY as `buggify!` and `hint!`, the simulated worker's `please_reboot` as attrition, and `fdbrpc` as the model for `moonpool-rpc`.

From **TigerBeetle**: the storage fault model, from torn and misdirected writes to phantom writes and sync failures, and the discipline of aiming faults so a replica set keeps a clean copy.

From **Antithesis**: the assertion taxonomy and the idea that a discovery is a point worth forking from.

From the paper behind **CLSTORE** (Alagappan et al., FAST '18): a journal that tells a crash from corruption and names what was damaged, built on the simulator's storage faults.

## The unifying philosophy

Across all three eras the philosophy is the same. Make execution **deterministic** so bugs are reproducible. Inject **faults** that are worse than production so a system that survives is overqualified for the real world. Explore **systematically** so bugs are found by infrastructure, not by imagination.

The tools change. The languages change. The accessibility changes. The philosophy does not.
