# Summary

# Foreword

- [State of Moonpool](./foreword/state.md)

---

# Part I: Why Simulation Testing

- [The Case for Simulation](./part1-why/case-for-simulation.md)
- [Prevention vs Discovery](./part1-why/prevention-vs-discovery.md)
- [From Mocks to Simulation](./part1-why/mocks-to-simulation.md)
- [A Brief History](./part1-why/history.md)
- [Why Moonpool Exists](./part1-why/why-moonpool.md)

---

# Part II: Foundations

- [Determinism as a Foundation](./part2-foundations/determinism.md)
  - [The Single-Core Constraint](./part2-foundations/single-core.md)
  - [Seed-Driven Reproducibility](./part2-foundations/seeds.md)
- [The Provider Pattern](./part2-foundations/provider-pattern.md)
  - [Quick Start: Swapping Implementations](./part2-foundations/providers-quickstart.md)
  - [Deep Dive: Why Providers Exist](./part2-foundations/providers-deepdive.md)
  - [The Five Providers](./part2-foundations/provider-traits.md)
- [System Under Test vs Test Driver](./part2-foundations/process-workload.md)
  - [Process: Your Server](./part2-foundations/process.md)
  - [Workload: Your Test Driver](./part2-foundations/workload.md)

---

# Part III: Building Simulations

- [Your First Simulation](./part3-building/first-simulation.md)
  - [Defining a Process](./part3-building/defining-process.md)
  - [Writing a Workload](./part3-building/writing-workload.md)
  - [Configuring the SimulationBuilder](./part3-building/simulation-builder.md)
  - [Running and Observing](./part3-building/running.md)
- [Chaos Testing vs Simulation](./part3-building/chaos-vs-simulation.md)
- [Chaos in Moonpool](./part3-building/chaos.md)
  - [Buggify: Fault Injection](./part3-building/buggify.md)
  - [Attrition: Process Reboots](./part3-building/attrition.md)
  - [Network Faults](./part3-building/network-faults.md)
  - [Storage Faults](./part3-building/storage-faults.md)
- [Assertions: Finding Bugs](./part3-building/assertions.md)
  - [Invariants vs Discovery vs Guidance](./part3-building/assertion-concepts.md)
  - [Always and Sometimes](./part3-building/always-sometimes.md)
  - [Numeric Assertions](./part3-building/numeric-assertions.md)
  - [Compound Assertions](./part3-building/compound-assertions.md)
- [System Invariants](./part3-building/invariants.md)
- [Debugging a Failing Seed](./part3-building/debugging.md)
  - [Reproducing with FixedCount](./part3-building/reproducing.md)
  - [Reading the Event Trace](./part3-building/event-trace.md)
  - [Common Pitfalls](./part3-building/pitfalls.md)

---

# Part IV: Networking and RPC

- [Simulating the Network](./part4-networking/simulating-network.md)
- [Peers and Connections](./part4-networking/peers.md)
  - [Backoff and Reconnection](./part4-networking/backoff.md)
  - [Wire Format](./part4-networking/wire-format.md)
- [RPC with #\[service\]](./part4-networking/rpc-service.md)
  - [Defining a Service](./part4-networking/defining-service.md)
  - [Server, Client, and Endpoints](./part4-networking/server-client-endpoints.md)

---

# Part V: Building on Top

- [Virtual Actors](./part5-building-on-top/virtual-actors.md)
  - [Turn-Based Concurrency](./part5-building-on-top/turn-based-concurrency.md)
  - [ActorHandler and Lifecycle](./part5-building-on-top/actor-handler.md)
  - [State Persistence](./part5-building-on-top/state-persistence.md)
  - [Placement and Directory](./part5-building-on-top/placement.md)
- [Multiverse Exploration](./part5-building-on-top/exploration.md)
  - [The Exploration Problem](./part5-building-on-top/exploration-problem.md)
  - [Fork at Discovery](./part5-building-on-top/fork-at-discovery.md)
  - [Coverage and Energy Budgets](./part5-building-on-top/coverage-energy.md)
  - [Adaptive Forking](./part5-building-on-top/adaptive-forking.md)
  - [Multi-Seed Exploration](./part5-building-on-top/multi-seed.md)

---

# Appendix

- [Assertion Reference](./appendix/assertion-reference.md)
- [Crate Map](./appendix/crate-map.md)
- [Configuration Reference](./appendix/configuration.md)
- [Glossary](./appendix/glossary.md)
