# Turn-Based Concurrency

<!-- toc -->

Concurrency bugs are some of the hardest to find and reproduce. Two threads writing to the same balance, a read that sees half-updated state, a lock ordering deadlock buried in a rarely-hit code path. Traditional shared-state concurrency demands constant vigilance from developers and constant luck from testers.

Virtual actors take a different approach: **one message at a time per identity**.

## The Rule

When a message arrives for `BankAccount("alice")`, the runtime guarantees that no other message for alice will be processed until the current one finishes. Bob's messages run independently. Charlie's messages run independently. But alice's messages are strictly sequential.

This is not a global lock. It is per-identity serialization. Different actor identities run concurrently. The same identity never runs concurrently with itself.

## Per-Identity Mailboxes

The mechanism is straightforward. Each actor identity gets its own `IdentityMailbox`, a simple queue with a waker:

```text
Routing Loop (per actor type)
  |
  +-- "alice" --> IdentityMailbox --> identity_processing_loop
  |
  +-- "bob"   --> IdentityMailbox --> identity_processing_loop
  |
  +-- "charlie" --> IdentityMailbox --> identity_processing_loop
```

The routing loop receives all incoming messages for an actor type (say, all `BankAccount` messages) and distributes them to the correct identity mailbox. Each identity has its own processing loop that dequeues one message at a time, dispatches it to the handler, sends the reply, then takes the next message.

The mailbox in moonpool uses `Rc<RefCell<VecDeque>>` with a `Waker`. No `Send` bounds, no `Arc`, no mutex. This follows the same pattern as `NetNotifiedQueue` in the transport layer, but without serialization since messages are already deserialized.

## Why Not a Single Sequential Loop?

An earlier version of the actor system had a single processing loop per actor **type**. All bank account messages went through one sequential loop. This created a deadlock: if actor A called actor B and they were the same type, the single loop blocked while waiting for B's response, but B's message was queued behind A's in-progress call.

The per-identity design eliminates this. Alice's loop can block waiting for Bob's response because Bob has his own independent loop. The loops are isolated, not sequential.

## The Tradeoff

Turn-based concurrency trades throughput for correctness:

**What we gain:**
- No data races within an actor. You can mutate `self.balance` freely.
- No locks. The runtime serialization means actors never need internal synchronization.
- Simpler reasoning. Each actor method runs from start to finish without interruption from other messages to the same identity.
- Better simulation focus. Since intra-actor concurrency bugs are impossible, chaos testing can focus entirely on inter-actor problems: message ordering, network partitions, node failures.

**What we give up:**
- A single hot actor becomes a bottleneck. If alice receives thousands of messages per second, they queue up. This is usually fine for entity-style actors (bank accounts, user profiles), but not for aggregation actors that process high-volume streams.
- Actor methods are async and can yield at `.await` points (for network calls, state persistence, etc.), but no other message for the same identity will run during those yields. This means a slow method blocks subsequent messages to that identity.

For simulation testing, this tradeoff is overwhelmingly positive. The actor model **eliminates an entire class of bugs** from within the actor, letting us direct all our chaos testing energy at the interactions **between** actors and the infrastructure they depend on. We do not need to test that alice's balance updates atomically. We need to test what happens when the node hosting alice crashes mid-transfer to bob.

## Comparison with Shared State

In a shared-state approach, you might protect `balance` with a mutex and spawn handlers on a thread pool. Every handler grabs the lock, does its work, releases it. This works, but:

- You need to verify the locking is correct (easy to introduce deadlocks or forgotten locks).
- Testing concurrent access requires precise thread scheduling, which is fragile and non-deterministic.
- Adding a network call while holding a lock is a recipe for deadlocks.

With actors, the scheduling is explicit and deterministic. The runtime decides the order. In simulation, that order is controlled by a seed, making every "concurrency" scenario reproducible.
