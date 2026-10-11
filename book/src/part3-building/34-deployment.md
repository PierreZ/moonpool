# Changing the Deployment Mid-Run

<!-- toc -->

The builder draws each group's boot shape once per seed. Real systems do not keep that shape. An operator racks new machines, decommissions old ones, and starts a new cell, all while clients run. A bug that only shows up while membership changes needs a simulation that changes membership.

Moonpool has no operator actor for this. Every workload gets one handle, `ctx.deployment()`, and an operator is an ordinary workload that uses it.

## The handle

```rust
let deployment = ctx.deployment();

// One new process in "server", booted from the group's factory.
let ip = deployment.spawn("server")?;                 // "10.0.1.4"

// The same, placed in a failure domain of a .cluster() group.
let ip = deployment.spawn_in("node", DomainSpec::NewZoneIn {
    datacenter: "dc1".to_string(),
})?;

// A whole new group on the next /24, ranks 0..3.
let ips = deployment.spawn_group("spare", 3, || Box::new(Spare))?;

// Stop a process for good.
deployment.retire("10.0.1.2")?;

// +1 on every spawn that boots and every retire.
let version = deployment.version();
```

| Call | What happens |
|---|---|
| `spawn(group)` | Reserves the next free host of the group's `/24` now, boots it one tick later from the group's factory. It takes the group's next round-robin tags. In a `.cluster()` group it gets a new machine in the zone of the group's highest member. |
| `spawn_in(group, domain)` | As `spawn`, placed by `DomainSpec`: `Machine(id)` (co-locate), `NewMachineIn { zone }`, `NewZoneIn { datacenter }`, `NewDatacenter`. New domain ids are never reused. |
| `spawn_group(name, count, factory)` | Declares a new group (the next `/24`) and spawns `count` processes from `factory`. The factory is `Send + Sync + 'static`. |
| `retire(ip)` | The process leaves every topology query and victim pool at once. One tick later it is killed. It never restarts, and its IP is never reused. Its disk stays readable for a checker (`FaultContext::durable_bytes`). |
| `version()` | The membership version, also on `topology().membership_version()` and `FaultContext::membership_version()`. |

A process gets a handle too, but every call returns an error: only a client changes the deployment. A hand-built `SimContext` carries a detached handle that refuses as well.

## The topology is live

`WorkloadTopology` and `FaultContext` read one shared `Membership` per iteration. They do not hold a copy taken at boot.

- `ips_in_group`, `all_process_ips`, `ips_tagged`, `ips_in_domain`, `group_for`, `tags_for` and `locality_for` include a spawned process once it boots and drop a retired one at once.
- A process's `client_id()` and `client_count()` are its rank and its group's size **now**. A spawned process takes the next rank. A retire moves the ranks above it down.
- `peer_by_name` and `peers()` list the entities present at boot only. Find spawned processes by group.

## Chaos sees new members

- **Attrition** draws its victim from the live pool at each decision, and `max_dead` counts against the live pool. A spawned process can be rebooted once it booted, never before.
- **Outages** pick their groups' live members when they fire.
- **Gray failure** keeps the windows it drew at the start of the chaos window. Once the deployment changed, a window that starts picks its victim from the live pool (one draw), so a spawned machine can go slow and a retired one never does.
- A fault aimed at a retired IP (`reboot`, `crash`, `restart`) is dropped.

## Determinism

Spawn and retire are simulation events (`ProcessSpawn`, `ProcessRetire`), handled by the orchestrator next to the restarts. The IP is reserved at the call. No randomness is drawn. So a seed spawns at the same time with the same IP on every replay, in the explorer's forks and under `check_determinism()`. The timeline records `process_spawn` and `process_retire` sim faults.

## Limits and the report

`SimulationBuilder::max_spawns(n)` caps the processes spawned per run (default 16). A spawn past the cap returns an error, so a buggy operator cannot fill a `/24`.

Each iteration's `SimulationMetrics::deployment` lists the spawned and retired IPs per group. The report prints a summary line:

```text
  Deployment   rack_node +140 -93 (spawned, retired)
```

## Example

`crates/moonpool-sim-examples/src/operator.rs` (`cargo xtask sim run operator`): an operator workload racks and decommissions nodes of a `.cluster()` group while a client pings the nodes the topology lists now, under attrition. `crates/moonpool-sim/tests/deployment.rs` pins each behavior.
