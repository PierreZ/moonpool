# Process Groups, Tags and Topology

<!-- toc -->

Our key-value simulation has one kind of server. Real systems have several: a consensus tier and a pool of spares, acceptors and the matchmakers that reconfigure them, a storage tier spread over three datacenters. The builder models all of that with three orthogonal tools. **Groups** say which role a process plays. **Tags** label processes inside a group. **Failure domains** say which processes share fate.

## Groups

Every `.processes()` call registers one **group**, named after its process type's `name()`. Each group has its own factory and its own per-seed count.

```rust
SimulationBuilder::new()
    .processes(3..=5, || Box::new(Acceptor))   // name() == "acceptor"
    .processes(0..=3, || Box::new(Matchmaker)) // name() == "matchmaker"
```

The counts are drawn from the simulation stream in registration order. Two things follow. A seed replays the same shape. And each role's size varies independently of the others', so one campaign covers three acceptors with no matchmaker and five acceptors with three.

Registering the same name twice panics: the name is the group's identity, so each role needs its own process type or its own `name()`.

### IP allocation

Every group owns its own `/24`. The *g*-th group registered, counting from zero, lives on `10.0.{g+1}.{1..=N}`. Workloads live on `10.0.0.x`.

| Entity | Addresses |
|---|---|
| workloads | `10.0.0.1`, `10.0.0.2`, ... |
| first group | `10.0.1.1` .. `10.0.1.N` |
| second group | `10.0.2.1` .. `10.0.2.M` |

A group's members are contiguous and recognisable at a glance, and a single-group builder keeps the `10.0.1.x` addresses it always had. The plan holds 255 groups of 255 processes. A group that draws **zero** processes this seed keeps its range: later groups do not shift down.

### Asking the topology

`ctx.topology()` answers group questions from a process or a workload:

| Call | Answer |
|---|---|
| `groups()` | every group name, in registration order, including groups that drew zero this seed |
| `ips_in_group("acceptor")` | the group's IPs, ascending, the caller included when it is a member |
| `group_for(ip)` | the group an IP belongs to |
| `my_group()` | the caller's own group, `None` for a workload |
| `all_process_ips()` | every process IP except the caller's own |

Inside a process, `ctx.client_id()` and `ctx.client_count()` are its index within and the size of **its own group**. An acceptor numbers itself `0..n` without knowing how many matchmakers this seed drew. The integration test `crates/moonpool-sim/tests/process_groups.rs` pins all of this:

```rust
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let topo = ctx.topology();
    let members = topo.ips_in_group(self.role);
    assert_always!(
        topo.my_group() == Some(self.role),
        "process_groups: a process knows its own group"
    );
    assert_always!(
        ctx.client_count() == members.len(),
        "process_groups: client_count is the group size"
    );
    assert_always!(
        members.get(ctx.client_id()).is_some_and(|ip| ip == ctx.my_ip()),
        "process_groups: client_id indexes the group's members"
    );
    // ...
}
```

A workload tells "no matchmakers this seed" from "this system has no matchmakers" by checking `groups()` against `ips_in_group("matchmaker")`.

### One fault regime per group

Groups are what role-aware faults aim at. Each `Chaos::Attrition` entry adds one injector, and `AttritionVictims::group(..)` scopes its draw **and** its `max_dead` budget to one pool:

```rust
.enable_chaos([
    Chaos::Attrition { config: regime(1, AttritionVictims::group("acceptor")), mode: ChaosMode::Random },
    Chaos::Attrition { config: regime(2, AttritionVictims::group("matchmaker")), mode: ChaosMode::Random },
])
.chaos_duration(Duration::from_secs(10))
```

At most one dead acceptor **and** at most two dead matchmakers, drawn independently. `Chaos::Outage` names groups the same way: `Outage::groups(["acceptor"])` crashes every acceptor at one instant. `ReplicatedFaults::new(DomainLevel::Zone).group("node")` spreads storage damage over one group's domains. A custom injector reads `ctx.ips_in_group(..)` on its `FaultContext`. [Attrition](./09-attrition.md), [Correlated Outages](./30-outages.md) and [Aiming Storage Faults](./33-aimed-faults.md) cover each.

## Tags

Tags label processes **inside** a group. `.tags()` attaches dimensions to the last `.processes()` or `.cluster()` call. Each dimension is distributed round-robin, independently, and each group's round-robin starts over.

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(ConsensusNode))
    .tags(&[
        ("role", &["leader", "follower"]),
        ("dc", &["east", "west", "eu"]),
    ])?
```

Process `i` gets `values[i % values.len()]` for each dimension: process 0 is `role=leader, dc=east`, process 1 is `role=follower, dc=west`, process 2 is `role=leader, dc=eu`, and so on. `.tags()` returns `Result<Self, SimulationError>` and fails with `InvalidState` when no group precedes it, so the call takes a `?` or an `expect`.

A process reads its own tags with `ctx.topology().my_tags().get("role")`. `ProcessTags::matches("role", "leader")` is the boolean form. The **TagRegistry** behind `ctx.topology()` maps every process IP to its tags:

```rust
let leaders = ctx.topology().ips_tagged("role", "leader");   // Vec<String>
let tags = ctx.topology().tags_for("10.0.1.2");               // Option<&ProcessTags>
```

The same registry serves fault injectors: `FaultContext::reboot_tagged("role", "leader", kind)` reboots every match, and `AttritionVictims::tagged("role", "voter")` scopes a built-in reboot regime by tag instead of by group. Tags are orthogonal to groups: a group is **which factory booted you**, a tag is **a label the builder handed you**. Use a group when the roles are different code, and a tag when one process type behaves differently by configuration.

## .cluster(): failure domains

`.processes()` places processes with no notion of where they run. `.cluster()` lays a group out across a FoundationDB-style hierarchy of datacenter, zone and machine, so collocated processes share fate.

```rust
use moonpool_sim::LocalityConfig;

SimulationBuilder::new()
    // 3 datacenters x 3 zones x 3 machines x 2 processes = 54 processes
    .cluster(LocalityConfig::new(3, 3, 3, 2), || Box::new(TopologyProcess))
```

The `LocalityConfig` **is** the spawn spec: the product of its four dimensions is the process count. Each dimension takes a fixed count or a range sampled per seed, so `LocalityConfig::new(1..=3, 3, 3, 1)` varies the datacenter count. Consecutive process indices share a machine, which is what makes a machine-scoped reboot meaningful. A `.cluster()` call is a group like any other: it gets its own IP range, its own name, and `.tags()` may still follow it.

### Zero-process draws

A dimension may draw zero, as in `LocalityConfig::new(0..=2, 1, 1, 1)`. The group is then empty for that seed: an optional role the deployment runs without. It gets no IPs. `ips_in_group(name)` is empty while `groups()` still lists it. Attrition finds no victim in it. A `ReplicatedFaults` scoped to it draws the `Spared` pattern. Every dimension is drawn in order even when an earlier one drew zero, so the shape never changes the draw schedule. The same holds for a plain `.processes(0..=3, ..)` group.

### Locality queries

With a cluster the topology answers where things are:

| Call | Answer |
|---|---|
| `my_locality()` | this process's `LocalityInfo` (`datacenter()`, `zone()`, `machine()`), `None` without `.cluster()` |
| `locality_for(ip)` | another process's locality |
| `peers_on_my_machine()` | the other processes on the caller's machine |
| `ips_in_domain(DomainLevel::Datacenter, "dc1")` | every process in a domain |

The topology example in `crates/moonpool-sim-examples/src/topology.rs` checks these on every boot:

```rust
let collocated = topo.peers_on_my_machine();
assert_always!(
    collocated.len() == PROCESSES_PER_MACHINE - 1,
    format!("expected {} collocated peer(s), saw {}", PROCESSES_PER_MACHINE - 1, collocated.len())
);
let in_dc = topo.ips_in_domain(DomainLevel::Datacenter, locality.datacenter());
assert_always!(in_dc.len() == PROCESSES_PER_DATACENTER, "every datacenter holds the same count");
```

Domain ids are `dc1`, `dc1-z2`, `dc1-z2-m3`: globally unique, so two datacenters never share a machine id.

### What a cluster unlocks

- **Correlated reboots.** `AttritionScope::PerMachine`, `PerZone` and `PerDatacenter` reboot every collocated process together, within the `max_dead` budget. Under `ChaosMode::Swarm` the regime picks a scope that fits the budget per seed.
- **Distance-dependent latency.** `.link_latency(LinkLatencyConfig::default())` keeps a cross-datacenter hop slow on a healthy seed. Pairs without locality, workload clients included, are unaffected.
- **Replicated fault patterns.** `.replicated_storage_faults(ReplicatedFaults::new(DomainLevel::Zone))` damages a replicated record in a minority of zones, in a stripe rotation, or one domain at a time, never everywhere.

`.processes()` and `.cluster()` mix on one builder, so a consensus tier can be clustered while its clients stay flat.

## Choosing Between Them

| Question | Tool |
|---|---|
| Different code, different factory, independent size | a group per `.processes()` call |
| Same code, a label that changes behaviour | `.tags()` on the group |
| Which processes die together | `.cluster()` with a `LocalityConfig` |

All three are visible to processes, workloads and fault injectors through the same registries, so a workload that targets "a follower in dc2 that shares a machine with a leader" composes `ips_tagged`, `ips_in_domain` and `peers_on_my_machine` without any new API.
