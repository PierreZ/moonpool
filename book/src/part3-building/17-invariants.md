# System Invariants

<!-- toc -->

Assertions live inside workloads. They validate local properties: "this balance should never go negative," "this response matched what we sent." But some correctness properties span the entire system. The conservation law in a banking system is a good example: the sum of all account balances must always equal total deposits minus total withdrawals. No single workload owns that property. We need something that watches the whole world.

That is what invariants are for.

## The Invariant Trait

An invariant is a check that runs **after every simulation event**. The simulation engine calls it automatically. If the invariant panics, the simulation stops and reports the failing seed.

```rust
pub trait Invariant: 'static {
    fn name(&self) -> &str;
    fn check(&self, state: &StateHandle, sim_time_ms: u64);
}
```

Two inputs: a `StateHandle` containing shared state that workloads publish, and the current simulation time. The contract is simple: if the invariant holds, return normally. If it does not, panic with a descriptive message.

You register invariants on the builder:

```rust
SimulationBuilder::new()
    .workload(BankingWorkload::new(100, accounts))
    .invariant(ConservationLaw)
    .invariant(NonNegativeBalances)
    .run()
    .await
```

For quick one-off checks, there is a closure shorthand:

```rust
SimulationBuilder::new()
    .invariant_fn("no_negative_balance", |state, _t| {
        if let Some(model) = state.get::<BankingModel>("banking_model") {
            for (account, balance) in &model.balances {
                assert!(*balance >= 0, "account '{}' went negative: {}", account, balance);
            }
        }
    })
```

## Sharing State with StateHandle

Invariants need to see what workloads are doing. `StateHandle` is the bridge. It is a type-safe, `Rc`-based key-value store that workloads publish into and invariants read from.

Workloads publish their state after each operation:

```rust
// Inside the workload's run() method
self.model.deposit(&account, amount);
ctx.state().publish("banking_model", self.model.clone());
```

Invariants read it back:

```rust
if let Some(model) = state.get::<BankingModel>("banking_model") {
    // validate...
}
```

The `if let Some` guard is important. Early in the simulation, before the workload has published anything, the key will not exist. Invariants should silently skip when their data is not yet available.

## A Real Example: The Conservation Law

Here is the conservation law invariant from Moonpool's banking simulation. It is the kind of property that catches subtle bugs: off-by-one in transfers, double-counting in deposits, missing updates in withdrawals.

```rust
pub struct ConservationLaw;

impl Invariant for ConservationLaw {
    fn name(&self) -> &str {
        "conservation_law"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<BankingModel>(BANKING_MODEL_KEY) {
            let total = model.total_balance();
            let expected = model.total_deposited - model.total_withdrawn;
            assert_always!(
                total == expected,
                format!(
                    "conservation law violated: sum(balances)={} != deposited({}) - withdrawn({})",
                    total, model.total_deposited, model.total_withdrawn
                )
            );
        }
    }
}
```

This runs after every single simulation event. If a transfer debits one account but crashes before crediting another, and the model does not account for it, this invariant fires immediately.

## When to Use Invariants vs Assertions

**Assertions** (`assert_always!`, `assert_sometimes!`) belong inside workloads. They validate local properties from the workload's perspective: "this response has the right balance," "this error path was exercised."

**Invariants** validate global, cross-workload properties from an omniscient perspective. They see the full system state and check that it is consistent. Use them for:

- Conservation laws (money, messages, resources)
- No-phantom properties (never receive something that was not sent)
- Consistency across actors (leader election: at most one leader at any time)
- Monotonicity properties (ballot numbers only increase)

A useful rule of thumb: if the property involves state from more than one actor or workload, it is an invariant. If it is about one workload's local view, it is an assertion.

## Performance

Invariants run after **every** simulation event. A typical simulation processes thousands of events per iteration, and you might run hundreds of iterations. Keep invariants fast.

Concretely: iterate a small collection, compare a few counters, check a simple predicate. Avoid expensive operations like sorting large datasets or doing string formatting on the happy path. The `format!` in the panic message is fine because it only runs when the invariant fails.

If you find yourself wanting a slow invariant (like replaying a log to verify consistency), consider running it only in the workload's `check()` method at the end of the simulation rather than after every event.
