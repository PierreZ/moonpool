# What's Next

<!-- toc -->

The transport you have read about in Part V is the post-refactor shape, the FDB-faithful core that took shape during the `transport_dx_again` round. A few features that earlier moonpool revisions shipped have been intentionally removed from the current crate to be rebuilt against this cleaner API. Three of them deserve a forward note so readers do not look for them and conclude they are missing.

## Load Balancing

FoundationDB's `loadBalance` keeps a list of `Alternatives` for the same logical service and picks among them based on queue depth, distance, and per-alternative health. Earlier moonpool versions shipped a port of this primitive. The version that existed coupled tightly to the pre-refactor `ServiceEndpoint<Req, Resp, C>` shape, so it was removed during Task 0 of the refactor rather than migrated through every API change. The pattern itself appears in chapter 10 as Strategy 6, and the building blocks (`get_reply_unless_failed_for`, `failure_monitor`, the `MaybeDelivered` propagation) are all in place. A future revision will reintroduce a load-balancing primitive against the unified `Calculator`-style interface.

## Fan-Out

Quorum reads, all-of writes, and race-the-fastest queries are common patterns in distributed systems. Earlier moonpool versions shipped `fan_out_quorum`, `fan_out_all`, and `fan_out_race` helpers. They were removed for the same reason as load balancing, and they will return alongside it in a future revision.

## Deterministic Simulation in the Transport Layer

The transport crate today runs against `TokioProviders` only. The simulation seam (the `TaskProvider` trait) is preserved, but the in-process simulation transport that earlier versions used to test the wire format under chaos was removed during Task 1 because it carried too many assertions tied to the old API. The `moonpool-sim` crate still simulates real time, randomness, storage, and a TCP-shape network. A future revision will reattach the transport crate to that simulation, restoring the chaos coverage that the earlier revisions had on packet drops, reordering, and partition events at the transport level.

## What Is Stable

Everything else in this part of the book is the shape we expect to keep: `#[service]`, the unified interface struct, `serve`, `init`, `from_base`, `well_known`, the four delivery modes, the failure monitor, the drop contract on `ReplyPromise`, and the address-plus-base-token serialization that lets interfaces flow as data. Build against these and the upcoming load-balance and fan-out primitives will compose on top without rewrites.
