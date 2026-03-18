# Summary

[Welcome](./welcome.md)
[Index](./index.md)

# Foreword

- [State of Moonpool](./foreword/01-state.md)

---

# Part I: Why Simulation Testing

- [The Case for Simulation](./part1-why/01-case-for-simulation.md)
- [Prevention vs Discovery](./part1-why/02-prevention-vs-discovery.md)
- [From Mocks to Simulation](./part1-why/03-mocks-to-simulation.md)
- [A Brief History](./part1-why/04-history.md)
- [Why Moonpool Exists](./part1-why/05-why-moonpool.md)

---

# Part II: Foundations

- [Determinism as a Foundation](./part2-foundations/01-determinism.md)
  - [The Single-Core Constraint](./part2-foundations/02-single-core.md)
  - [Seed-Driven Reproducibility](./part2-foundations/03-seeds.md)
- [The Provider Pattern](./part2-foundations/04-provider-pattern.md)
  - [Quick Start: Swapping Implementations](./part2-foundations/05-providers-quickstart.md)
  - [Deep Dive: Why Providers Exist](./part2-foundations/06-providers-deepdive.md)
  - [The Five Providers](./part2-foundations/07-provider-traits.md)
- [System Under Test vs Test Driver](./part2-foundations/08-process-workload.md)
  - [Process: Your Server](./part2-foundations/09-process.md)
  - [Workload: Your Test Driver](./part2-foundations/10-workload.md)

---

# Part III: Building Simulations

- [Your First Simulation](./part3-building/01-first-simulation.md)
  - [Defining a Process](./part3-building/02-defining-process.md)
  - [Writing a Workload](./part3-building/03-writing-workload.md)
  - [Configuring the SimulationBuilder](./part3-building/04-simulation-builder.md)
  - [Running and Observing](./part3-building/05-running.md)
- [Chaos Testing vs Simulation](./part3-building/06-chaos-vs-simulation.md)
- [Chaos in Moonpool](./part3-building/07-chaos.md)
  - [Buggify: Fault Injection](./part3-building/08-buggify.md)
  - [Attrition: Process Reboots](./part3-building/09-attrition.md)
  - [Network Faults](./part3-building/10-network-faults.md)
  - [Storage Faults](./part3-building/11-storage-faults.md)
- [Assertions: Finding Bugs](./part3-building/12-assertions.md)
  - [Invariants vs Discovery vs Guidance](./part3-building/13-assertion-concepts.md)
  - [Always and Sometimes](./part3-building/14-always-sometimes.md)
  - [Numeric Assertions](./part3-building/15-numeric-assertions.md)
  - [Compound Assertions](./part3-building/16-compound-assertions.md)
- [System Invariants](./part3-building/17-invariants.md)
- [Event Timelines](./part3-building/18-event-timelines.md)
- [Designing Workloads That Find Bugs](./part3-building/19-designing-workloads.md)
- [Debugging a Failing Seed](./part3-building/20-debugging.md)
  - [Reproducing with FixedCount](./part3-building/21-reproducing.md)
  - [Reading the Event Trace](./part3-building/22-event-trace.md)
  - [Common Pitfalls](./part3-building/23-pitfalls.md)

---

# Part IV: Simulating Existing Applications

- [Using moonpool-sim Standalone](./part4-integration/01-standalone-sim.md)
  - [Where to Draw the Line](./part4-integration/02-mock-boundaries.md)
  - [Wiring a Web Service](./part4-integration/03-wiring-a-web-service.md)
  - [What You're Testing (and What You're Not)](./part4-integration/04-scope-and-tradeoffs.md)

---

# Part V: Networking and RPC

- [Simulating the Network](./part4-networking/01-simulating-network.md)
- [Peers and Connections](./part4-networking/02-peers.md)
  - [Backoff and Reconnection](./part4-networking/03-backoff.md)
  - [Wire Format](./part4-networking/04-wire-format.md)
- [RPC with #\[service\]](./part4-networking/05-rpc-service.md)
  - [Defining a Service](./part4-networking/06-defining-service.md)
  - [Server, Client, and Endpoints](./part4-networking/07-server-client-endpoints.md)
- [Delivery Modes](./part4-networking/08-delivery-modes.md)
- [Failure Monitor](./part4-networking/09-failure-monitor.md)
- [Designing Simulation-Friendly RPC](./part4-networking/10-designing-rpc.md)

---

# Part VI: Building on Top

- [Multiverse Exploration](./part5-building-on-top/01-exploration.md)
  - [The Exploration Problem](./part5-building-on-top/02-exploration-problem.md)
  - [Fork at Discovery](./part5-building-on-top/03-fork-at-discovery.md)
  - [Coverage and Energy Budgets](./part5-building-on-top/04-coverage-energy.md)
  - [Adaptive Forking](./part5-building-on-top/05-adaptive-forking.md)
  - [Multi-Seed Exploration](./part5-building-on-top/06-multi-seed.md)

---

# Appendix

- [Assertion Reference](./appendix/01-assertion-reference.md)
- [Crate Map](./appendix/02-crate-map.md)
- [Configuration Reference](./appendix/03-configuration.md)
- [Fault Reference](./appendix/04-fault-reference.md)
- [Glossary](./appendix/05-glossary.md)
