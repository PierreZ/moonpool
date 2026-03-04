# Virtual Actors

<!-- toc -->

**This is the most experimental part of moonpool.** The virtual actor layer exists primarily as a proving ground: a non-trivial programming model built on top of the simulation framework, used to stress-test the framework itself. It is inspired by Microsoft's Orleans and by curiosity about how actors compose with deterministic simulation. The APIs here are the most likely to change.

We have a simulation runtime that controls time, a network layer that moves bytes between peers, and an RPC system that turns those bytes into method calls. That is enough to build distributed systems. But most distributed systems are not built around raw RPC calls. They are built around **entities**: bank accounts, user sessions, device controllers, game players. Each entity has an identity, some state, and a set of operations. The question is how we model those entities in code.

## The Problem with Manual Lifecycle

One approach is to manage entity instances yourself. You create an object for each bank account, keep it in a HashMap, handle incoming requests, and look up the right object to dispatch to. When memory runs low you evict instances, serialize their state, and reload later. When a node crashes you need to figure out which entities lived there and re-create them elsewhere.

This is tedious, error-prone, and has nothing to do with your actual business logic. It is plumbing.

## The Orleans Model

Virtual actors (pioneered by Microsoft's Orleans framework) solve this with three principles:

**Identity-based**: Every actor has a unique identity, like `BankAccount("alice")`. You never create or destroy actors explicitly. You just address them by identity and they exist.

**Location-transparent**: The caller does not know or care which node hosts the actor. You say "send a deposit to BankAccount alice" and the system figures out where alice lives, activating her on a node if needed.

**Auto-lifecycle**: Actors are activated on first message and deactivated when idle. No manual lifecycle management. The runtime handles creation, placement, and garbage collection.

In moonpool, we implement this pattern on top of the transport layer. The transport moves bytes between endpoints. The actor layer adds identity, directory lookup, and method dispatch.

```text
+-------------------------------------------------------+
|               Virtual Actor Layer                      |
|  ActorRouter -> Directory -> PlacementDirector         |
|  ActorMessage carries identity + method + body         |
+-------------------------------------------------------+
|               Transport Layer                          |
|  NetTransport -> EndpointMap -> Peer connections       |
|  Token 0 reserved for actor dispatch per type          |
+-------------------------------------------------------+
```

## Why Actors for Simulation

Virtual actors are a natural fit for deterministic simulation testing. Consider what we want to test in a distributed bank account system:

- What happens when a deposit message arrives at a node that just rebooted?
- What happens when two nodes both try to activate the same actor?
- What happens when a node dies while an actor is persisting state?

With virtual actors, each entity maps to a clear identity. The simulation can target chaos at specific actors (kill the node hosting alice), and our assertions can reason about actor-level invariants (alice's balance should never go negative). The identity-based model makes both fault injection and correctness checking more natural than reasoning about raw socket connections.

The other major benefit is **turn-based concurrency**. Each actor processes one message at a time, which eliminates data races within an actor. This means simulation testing can focus entirely on **inter-actor** behavior: message ordering, network partitions, and node failures. We will cover turn-based concurrency in detail in the next chapter.

## The Building Blocks

The virtual actor system has several components that we will explore across the next chapters:

- **ActorHandler**: the trait that defines an actor's behavior, lifecycle hooks, and dispatch
- **ActorHost**: the server-side runtime that owns actor instances and runs processing loops
- **ActorRouter**: the caller-side component that resolves actor locations and sends requests
- **ActorDirectory**: the phone book that maps actor identities to network endpoints
- **PlacementDirector**: the algorithm that decides where to activate new actors
- **PersistentState**: typed, ETag-guarded state that survives actor deactivation

Together, these let you write code like this:

```rust
let alice: BankAccountRef<_> = node.actor_ref("alice");
let resp = alice.deposit(DepositRequest { amount: 100 }).await?;
```

Behind that simple call, the system looks up alice in the directory, places her on a node if she does not exist yet, routes the message, activates the actor, loads her state, processes the deposit, persists the result, and returns the response. All under deterministic simulation control.
