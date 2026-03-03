# Placement and Directory

<!-- toc -->

When you call `alice.deposit(req).await`, the system needs to answer two questions: **where is alice?** and if she does not exist yet, **where should she be created?** The directory answers the first question. The placement director answers the second.

## ActorDirectory

The `ActorDirectory` is the actor system's phone book. It maps `ActorId` to `ActorAddress`, which combines the identity, the endpoint where the actor is hosted, and a unique `ActivationId`:

```rust
#[async_trait(?Send)]
pub trait ActorDirectory: fmt::Debug {
    async fn lookup(&self, id: &ActorId) -> Result<Option<ActorAddress>, DirectoryError>;
    async fn register(&self, address: ActorAddress) -> Result<ActorAddress, DirectoryError>;
    async fn unregister(&self, address: &ActorAddress) -> Result<(), DirectoryError>;
    async fn unregister_members(&self, addresses: &[NetworkAddress])
        -> Result<Vec<ActorAddress>, DirectoryError>;
}
```

The `register` method follows Orleans semantics: if no entry exists, it registers the new address and returns it. If an entry **already exists**, it returns the **existing** entry without overwriting. The caller compares activation IDs to detect conflicts.

This matters during races. Two nodes might both try to activate alice simultaneously. The first one to register wins. The second sees the existing entry and knows to forward or back off.

`unregister` only removes an entry if the activation ID matches, preventing a node from accidentally removing a newer activation that replaced it. `unregister_members` is a batch cleanup for dead nodes, removing all actors that were hosted there so they can be re-activated elsewhere.

`InMemoryDirectory` implements this with a simple `HashMap<ActorId, ActorAddress>`. For simulation, all nodes share the same directory instance via `Rc`, giving them a consistent view.

## PlacementDirector

When an actor is not in the directory, the `PlacementDirector` decides which node should host it:

```rust
#[async_trait(?Send)]
pub trait PlacementDirector: fmt::Debug {
    async fn place(
        &self,
        strategy: PlacementStrategy,
        id: &ActorId,
        active_members: &[NetworkAddress],
        local_address: &NetworkAddress,
    ) -> Result<Endpoint, PlacementError>;
}
```

The director receives a **strategy hint** from the actor type, the list of active cluster members, and the local node's address. It returns the endpoint where the actor should be activated.

`PlacementStrategy` is a per-actor-type enum:

- **`Local`** (default): activate on the node that first sends the message. Good for actors that are naturally colocated with their callers.
- **`RoundRobin`**: distribute across active cluster members. Good for spreading load across a cluster.

The banking example declares round-robin placement:

```rust
impl ActorHandler for BankAccountImpl {
    fn placement_strategy() -> PlacementStrategy {
        PlacementStrategy::RoundRobin
    }
}
```

`DefaultPlacementDirector` handles both strategies. `Local` returns the caller's own address. `RoundRobin` cycles through active members with a simple counter.

The separation between strategy (what the actor wants) and director (how the cluster satisfies it) follows Orleans' design. You can implement a custom `PlacementDirector` for more sophisticated algorithms like consistent hashing or affinity-based placement.

## MembershipProvider

The placement director needs to know which nodes are alive. `MembershipProvider` provides this view:

```rust
#[async_trait(?Send)]
pub trait MembershipProvider: fmt::Debug {
    async fn members(&self) -> Vec<NetworkAddress>;
    async fn snapshot(&self) -> MembershipSnapshot;
    async fn register_node(&self, address: NetworkAddress, status: NodeStatus, name: String)
        -> Result<MembershipVersion, MembershipError>;
    async fn update_status(&self, address: &NetworkAddress, status: NodeStatus)
        -> Result<MembershipVersion, MembershipError>;
}
```

Nodes self-register during startup and transition through `Joining`, `Active`, `ShuttingDown`, and `Dead` states. Only `Active` nodes appear in `members()`, which is what placement uses.

`SharedMembership` is the simulation implementation. All nodes share a single `Rc<SharedMembership>`, seeing membership changes immediately. In production, this would be backed by a gossip protocol or shared storage.

## The Request Flow

Here is the full flow when a caller sends a message to an actor:

1. **ActorRouter** checks the directory for the target actor
2. If found, the message goes to the cached endpoint
3. If not found, the router asks the `PlacementDirector` to pick a node
4. The message is sent to the chosen node via the transport
5. The receiving node's **ActorHost** looks up the local identity
6. If the actor is not active locally, it creates the instance, calls `on_activate`, and **registers in the directory** (this is when the directory entry is created)
7. The message is dispatched to the handler and the response returns to the caller

Directory registration happens at the **target host**, not the caller. This is the Orleans model: the node that activates the actor is the one that registers it.

## Forwarding

What if the directory is stale? Node A thinks alice is on Node B, but she was deactivated and re-activated on Node C. The message arrives at Node B, which does not have alice. Rather than failing, Node B can **forward** the message.

Messages carry a `forward_count` field (max 2 hops). If a node receives a message for an actor it does not host, it increments the counter, re-resolves through the directory, and forwards. The response includes a `CacheInvalidation` hint so the original caller can update its directory cache.

## Putting It Together

The `MoonpoolNode` builder wires all these pieces:

```rust
let membership = Rc::new(SharedMembership::new());
let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

let cluster = ClusterConfig::builder()
    .name("banking")
    .membership(membership.clone())
    .directory(directory.clone())
    .build()?;

let node = MoonpoolNode::new(cluster, NodeConfig::builder()
    .address(addr)
    .state_store(state_store.clone())
    .build())
    .with_providers(providers)
    .register::<BankAccountImpl>()
    .start()
    .await?;

let alice: BankAccountRef<_> = node.actor_ref("alice");
alice.deposit(DepositRequest { amount: 100 }).await?;
```

`ClusterConfig` bundles the shared resources (directory, membership, placement director). `NodeConfig` holds per-node settings (address, state store). Multiple nodes share the same `ClusterConfig` but each has its own `NodeConfig` with a unique address.

After `start()`, the node registers itself in membership as `Active`, creates its transport and router, and spawns processing loops for each registered actor type. At this point, `node.actor_ref("alice")` creates a typed reference that knows how to route messages through the directory and placement system to reach alice, wherever she may live.
