# Placement and Directory

<!-- toc -->

- `ActorDirectory`: resolves ActorAddress → Endpoint (where is this actor?)
- `PlacementDirector`: decides where to activate new actors
- `MembershipProvider`: cluster topology — which nodes are alive?
- `MembershipSnapshot`: tracks nodes and their status
- `ActorRef<T>`: typed reference for calling remote actors
- Forwarding: `MAX_FORWARD_COUNT=2` for actor migration during calls
