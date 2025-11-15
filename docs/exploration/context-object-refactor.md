# Context Object Refactor Exploration

## Current State: Provider Trait Pattern

### Architecture Overview

The foundation crate currently uses a **provider trait pattern** where different capabilities (network, time, random, task) are abstracted through traits:

```rust
pub trait NetworkProvider: Clone { ... }
pub trait TimeProvider: Clone { ... }
pub trait TaskProvider: Clone { ... }
pub trait RandomProvider: Clone { ... }
```

These providers are passed as generic parameters throughout the codebase.

### Complexity Analysis

#### 1. Generic Parameter Explosion

**Example from `Peer`:**
```rust
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,
    // ... other fields
}
```

**Example from Actor:**
```rust
pub struct PingPongServerActor<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
> {
    network: N,
    time: T,
    task_provider: TP,
    // ... other fields
}
```

**Problems:**
- Every struct that needs multiple providers requires 3+ generic parameters
- Trait bounds must be repeated everywhere: `Clone + 'static`
- Type signatures become verbose and hard to read
- Adding a new provider type requires updating every struct signature

#### 2. Provider Storage Redundancy

Each struct stores individual provider instances:
```rust
struct MyActor<N, T, TP> {
    network: N,      // One field
    time: T,         // Another field
    task_provider: TP, // Yet another field
    // Plus actual state...
}
```

#### 3. Construction Complexity

Creating instances requires passing all providers:
```rust
let actor = PingPongServerActor::new(
    network,      // Parameter 1
    time,         // Parameter 2
    task_provider, // Parameter 3
    topology,     // Actual domain parameter
);
```

#### 4. Method Signatures

Functions that work with these types become complex:
```rust
async fn connection_task<
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
>(
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,
    // ...
)
```

### Current Benefits

1. **Compile-time polymorphism** - Zero runtime cost through monomorphization
2. **Type safety** - Different provider implementations are distinct types
3. **Explicit dependencies** - Clear what capabilities each component uses
4. **No trait objects** - Avoids dynamic dispatch overhead

---

## Proposed: Context Object Pattern

### Core Idea

Replace the provider traits with a single **cloneable Context object** that contains all providers:

```rust
#[derive(Clone)]
pub struct SimContext {
    pub network: Arc<dyn NetworkProvider>,
    pub time: Arc<dyn TimeProvider>,
    pub task: Arc<dyn TaskProvider>,
    pub random: Arc<dyn RandomProvider>,
}

#[derive(Clone)]
pub struct TokioContext {
    pub network: TokioNetworkProvider,
    pub time: TokioTimeProvider,
    pub task: TokioTaskProvider,
    // No random needed in production
}

// Unified trait for both
pub trait Context: Clone {
    type Network: NetworkProvider;
    type Time: TimeProvider;
    type Task: TaskProvider;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
}
```

### Alternative: Concrete Context Enum

```rust
#[derive(Clone)]
pub enum Ctx {
    Sim(SimCtx),
    Tokio(TokioCtx),
}

#[derive(Clone)]
pub struct SimCtx {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: SimTaskProvider,
    random: SimRandomProvider,
}

#[derive(Clone)]
pub struct TokioCtx {
    network: TokioNetworkProvider,
    time: TokioTimeProvider,
    task: TokioTaskProvider,
}

impl Ctx {
    pub fn network(&self) -> &dyn NetworkProvider {
        match self {
            Ctx::Sim(ctx) => &ctx.network,
            Ctx::Tokio(ctx) => &ctx.network,
        }
    }
    // Similar for time, task, etc.
}
```

### Alternative: Zero-Cost Context with Generics

Keep zero-cost abstraction but simplify the interface:

```rust
pub struct Ctx<N, T, TP, R> {
    pub network: N,
    pub time: T,
    pub task: TP,
    pub random: R,
}

// Convenience type aliases
pub type SimCtx = Ctx<
    SimNetworkProvider,
    SimTimeProvider,
    SimTaskProvider,
    SimRandomProvider,
>;

pub type TokioCtx = Ctx<
    TokioNetworkProvider,
    TokioTimeProvider,
    TokioTaskProvider,
    (),  // No random in production
>;
```

**Usage:**
```rust
// Before: 3-4 generic parameters
struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> { }

// After: 1 generic parameter
struct Peer<C: Context> {
    ctx: C,
}

// Or with concrete types
struct Peer {
    ctx: SimCtx,  // Or TokioCtx
}
```

---

## Detailed Comparison

### Code Simplification Examples

#### Actor Definition

**Before:**
```rust
pub struct PingPongServerActor<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
> {
    network: N,
    time: T,
    task_provider: TP,
    topology: WorkloadTopology,
    pings_received: usize,
}
```

**After (with Context):**
```rust
pub struct PingPongServerActor<C: Context> {
    ctx: C,
    topology: WorkloadTopology,
    pings_received: usize,
}

// Or even simpler with concrete type:
pub struct PingPongServerActor {
    ctx: SimCtx,  // Or Ctx enum
    topology: WorkloadTopology,
    pings_received: usize,
}
```

#### Construction

**Before:**
```rust
let actor = PingPongServerActor::new(
    network,
    time,
    task_provider,
    topology,
);
```

**After:**
```rust
let actor = PingPongServerActor::new(
    ctx,      // Single parameter!
    topology,
);
```

#### Usage

**Before:**
```rust
self.time.sleep(Duration::from_millis(100)).await?;
self.network.connect("127.0.0.1:8080").await?;
self.task_provider.spawn_task("worker", async { ... });
```

**After:**
```rust
self.ctx.time.sleep(Duration::from_millis(100)).await?;
self.ctx.network.connect("127.0.0.1:8080").await?;
self.ctx.task.spawn_task("worker", async { ... });
```

---

## Trade-offs Analysis

### Option 1: Trait Object Context (Dynamic Dispatch)

```rust
pub struct SimContext {
    pub network: Arc<dyn NetworkProvider>,
    pub time: Arc<dyn TimeProvider>,
    pub task: Arc<dyn TaskProvider>,
}
```

**Pros:**
- ✅ Maximum simplification - no generics needed
- ✅ Single concrete type
- ✅ Easy to add new provider types
- ✅ Smaller binary size (no monomorphization)
- ✅ Faster compile times

**Cons:**
- ❌ Runtime cost of dynamic dispatch
- ❌ Heap allocation for Arc
- ❌ Cannot use const generics or associated types easily
- ❌ Loses compile-time type checking between sim/prod

### Option 2: Context Trait with Generics

```rust
pub trait Context: Clone {
    type Network: NetworkProvider;
    type Time: TimeProvider;
    // ...
}
```

**Pros:**
- ✅ Zero runtime cost (still monomorphized)
- ✅ Compile-time type safety
- ✅ Reduces parameter count from 3-4 to 1
- ✅ Still explicit about provider types

**Cons:**
- ❌ Still have generics (though fewer)
- ❌ Larger binary from monomorphization
- ❌ Some complexity remains

### Option 3: Concrete Context Enum

```rust
pub enum Ctx {
    Sim(SimCtx),
    Tokio(TokioCtx),
}
```

**Pros:**
- ✅ No generics needed in most code
- ✅ Single concrete type
- ✅ Pattern matching for sim vs prod
- ✅ Fast dispatch (enum, not trait object)

**Cons:**
- ❌ Small runtime cost (enum tag check)
- ❌ Must handle both variants everywhere
- ❌ Cannot easily extend to new context types

### Option 4: Generic Context Struct (Zero-Cost)

```rust
pub struct Ctx<N, T, TP, R> {
    pub network: N,
    pub time: T,
    pub task: TP,
    pub random: R,
}
```

**Pros:**
- ✅ Zero runtime cost
- ✅ Reduces 4 generic parameters to 1 in consuming code
- ✅ Type aliases make it ergonomic: `SimCtx`, `TokioCtx`
- ✅ Maintain full type safety
- ✅ Easy to understand - just a struct with fields

**Cons:**
- ❌ Still requires generics in the Ctx definition itself
- ❌ Type aliases can hide complexity
- ❌ Each new provider requires updating Ctx generic params

---

## Performance Considerations

### Simulation Context

For simulation, performance is **not critical** because:
1. The simulation is deterministic, not production
2. Correctness and debuggability are more important
3. Tests don't need microsecond-level performance
4. Dynamic dispatch cost is negligible compared to the simulation overhead

**Recommendation:** Dynamic dispatch is acceptable for sim context.

### Production Context (Tokio)

For production, performance **matters more** because:
1. Real production workloads
2. High-frequency operations (sleep, spawn_task)
3. Network hot paths

**However:**
- Modern CPUs handle indirect calls well (branch prediction)
- Network I/O dominates any dispatch overhead
- Smart inlining can eliminate virtual calls

**Recommendation:** Measure before optimizing. Start with enum dispatch, profile, then decide.

---

## Migration Path

### Phase 1: Create Context Types

```rust
// Add to foundation/src/context.rs

pub mod context {
    // Start with zero-cost option
    #[derive(Clone)]
    pub struct Ctx<N, T, TP> {
        pub network: N,
        pub time: T,
        pub task: TP,
    }

    pub type SimCtx = Ctx<
        SimNetworkProvider,
        SimTimeProvider,
        SimTaskProvider,
    >;

    pub type TokioCtx = Ctx<
        TokioNetworkProvider,
        TokioTimeProvider,
        TokioTaskProvider,
    >;
}
```

### Phase 2: Convert One Module

Pick a small module (e.g., `Peer`) and convert it:

```rust
// Before
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    network: N,
    time: T,
    task_provider: TP,
}

// After
pub struct Peer<C> where C: Clone {
    ctx: C,
}

// Or even simpler - no generics at all!
pub struct Peer {
    ctx: SimCtx,
}
```

### Phase 3: Measure & Evaluate

- Compare binary size
- Measure compilation time
- Run benchmarks
- Assess code readability

### Phase 4: Decide & Roll Out

Based on results, either:
- Continue with zero-cost generic Ctx
- Switch to enum-based Ctx if generics still problematic
- Switch to trait objects if performance is acceptable

---

## Recommendation

### Best Approach: Generic Context Struct with Type Aliases

Use **Option 4** - Generic Context Struct with type aliases:

```rust
#[derive(Clone)]
pub struct Ctx<N, T, TP> {
    pub network: N,
    pub time: T,
    pub task: TP,
}

pub type SimCtx = Ctx<
    SimNetworkProvider,
    SimTimeProvider,
    SimTaskProvider,
>;

pub type TokioCtx = Ctx<
    TokioNetworkProvider,
    TokioTimeProvider,
    TokioTaskProvider,
>;
```

**Rationale:**

1. **Zero runtime cost** - Maintains current performance characteristics
2. **Massive simplification** - Reduces 3-4 generic params to 1
3. **Type safety** - Compile-time guarantees remain
4. **Ergonomic** - Type aliases make usage clean: `ctx: SimCtx`
5. **Backwards compatible** - Can gradually migrate
6. **Future flexible** - Easy to switch to enum/trait object later if needed

### Usage Pattern

**In tests/simulation:**
```rust
struct MyActor {
    ctx: SimCtx,  // Concrete type, no generics!
    // ... state
}

impl MyActor {
    pub fn new(ctx: SimCtx) -> Self {
        Self { ctx }
    }

    pub async fn run(&mut self) {
        self.ctx.time.sleep(Duration::from_millis(100)).await;
        self.ctx.network.connect("...").await;
        self.ctx.task.spawn_task("worker", async { ... });
    }
}
```

**In library code (generic):**
```rust
struct Peer<C> {
    ctx: C,
}

impl<C> Peer<C>
where
    C: Clone,
    // Could add trait bounds here if needed
{
    pub fn new(ctx: C) -> Self {
        Self { ctx }
    }
}
```

### Random Provider Integration

Add `random` to the context:

```rust
pub struct Ctx<N, T, TP, R = ()> {
    pub network: N,
    pub time: T,
    pub task: TP,
    pub random: R,  // Optional with default
}

pub type SimCtx = Ctx<
    SimNetworkProvider,
    SimTimeProvider,
    SimTaskProvider,
    SimRandomProvider,
>;

pub type TokioCtx = Ctx<
    TokioNetworkProvider,
    TokioTimeProvider,
    TokioTaskProvider,
    (),  // No random in prod
>;
```

---

## Implementation Checklist

- [ ] Create `src/context.rs` with `Ctx` struct
- [ ] Add `SimCtx` and `TokioCtx` type aliases
- [ ] Migrate `Peer` to use `Ctx`
- [ ] Migrate `ClientTransport` to use `Ctx`
- [ ] Migrate `ServerTransport` to use `Ctx`
- [ ] Migrate test actors to use `SimCtx`
- [ ] Update documentation
- [ ] Run full test suite
- [ ] Measure binary size impact
- [ ] Measure compilation time impact
- [ ] Decide on final approach based on metrics

---

## Open Questions

1. **Should RandomProvider be in Context?**
   - It's only used in simulation, not production
   - Could make it an optional generic with default: `R = ()`
   - Or keep it separate as thread-local (current approach)

2. **Should Context be a trait or just a struct?**
   - Struct is simpler and sufficient
   - Trait adds flexibility but complexity
   - Start with struct, add trait only if needed

3. **Should we support building custom contexts?**
   - E.g., for testing with mock providers
   - Builder pattern might be useful
   - Start simple, add as needed

4. **How to handle WeakSimWorld?**
   - SimTimeProvider needs WeakSimWorld
   - Could add to SimCtx as well
   - Or keep time provider as-is with weak reference

---

## Conclusion

The **Context Object Pattern with Generic Ctx and Type Aliases** offers the best balance:

- **Massive code simplification** - Single parameter instead of 3-4
- **Zero runtime cost** - Full compile-time optimization
- **Excellent ergonomics** - Clean, readable code
- **Type safety** - Compile-time guarantees maintained
- **Gradual migration** - Can convert module by module

This addresses the complexity concern while maintaining all the benefits of the current provider pattern.
