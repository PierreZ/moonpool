# Exploration: Context Object Pattern

This directory contains an exploration of replacing the Provider trait pattern with a Context object pattern in the moonpool foundation crate.

## Documents

### [context-evaluation.md](context-evaluation.md) - **START HERE**
The main analysis with my recommendation. Evaluates whether Context would actually help moonpool.

**TL;DR:** Not recommended. The complexity is contained to 3 library structs, and Context would trade known issues for new ones. Better to make test actors use concrete types directly.

### [context-object-refactor.md](context-object-refactor.md)
Detailed technical analysis of the Context pattern:
- Current provider architecture
- Four different Context implementation approaches
- Trade-off analysis
- Migration path

### [context-poc.rs](context-poc.rs)
Proof-of-concept code showing:
- Current approach vs proposed approach side-by-side
- Concrete examples of code reduction
- Three implementation variants (generic struct, enum, trait object)
- Metrics comparison

## Summary

**Problem:** Provider trait generics create verbose type signatures like `Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider>`.

**Proposed Solution:** Bundle providers into a Context object: `Peer<C>` or `Peer { ctx: SimCtx }`.

**Reality Check:**
- Only 3 structs in moonpool use multi-provider generics (Peer, ClientTransport, ServerTransport)
- Test actors are generic in definition but instantiated with concrete types
- RandomProvider isn't threaded through structs - it's thread-local
- User-facing API already uses concrete types via WorkloadFn

**Recommendation:** Don't refactor to Context. Instead, make test actors use concrete types directly:

```rust
// Instead of:
pub struct PingPongServerActor<N, T, TP> { ... }

// Just use:
pub struct PingPongServerActor {
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task: TokioTaskProvider,
    // ...
}
```

This achieves 90% of Context's benefits with 10% of the work and zero new concepts.

## Key Findings

1. **Complexity is contained** - Only 3 library structs have the full provider pattern
2. **User code already concrete** - WorkloadFn receives concrete provider types
3. **RandomProvider doesn't exist in structs** - Thread-local, not passed around
4. **Migration cost > benefit** - Rewriting working code for marginal gains
5. **Trade-offs unclear** - Lose explicitness and flexibility

## When Context WOULD Make Sense

- 10+ structs with multi-provider generics (moonpool has 3)
- RandomProvider threaded everywhere (it's thread-local)
- Frequent provider additions (rare in moonpool)
- User complaints about API (none reported)
- Shared actors between sim and production (sim-only currently)

None of these apply to moonpool.

## Decision

**Do not pursue Context object refactor at this time.**

Focus on shipping features instead. Revisit if real pain emerges from users.
