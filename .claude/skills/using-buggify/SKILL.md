---
name: using-buggify
description: Place and calibrate moonpool fault injection - buggify!(), buggify_with_prob!(p), buggify_knob!(default, lo..hi), the per-seed activation × per-call firing model, Chaos::BuggifyKnobs, and the FoundationDB placement rules (optional work, error paths, concurrency windows, tuning knobs). Use when adding an injection point, when a rare-but-valid state needs to become likely, when turning a constant into a per-seed tunable, or when deciding why a sometimes gate never fires.
---

# Using buggify

Random network and storage faults reach what the swarm stumbles into. Buggify
reaches what you already know is rare and valid: a batch that is skipped, a
retry that fires spuriously, a queue of size one, a window stretched wide.
Each site is inert in production (the macros return `false` / the default
unless moonpool-sim installed its seeded source), so removing every site
leaves the shipped program unchanged.

## The macros

```rust
use moonpool_sim::{buggify, buggify_with_prob, buggify_knob};

if buggify!() { return Err(Error::timeout("simulated")); }          // 25% firing when the site is active
if buggify_with_prob!(0.05) { time.sleep(extra).await?; }            // your own firing rate
let batch = buggify_knob!(64_usize, 1_usize..8_usize);              // default, or a per-seed extreme in lo..hi
```

- `buggify!` and `buggify_with_prob!` live in the zero-dependency
  `moonpool-buggify` crate (`crates/moonpool-buggify/src/lib.rs`) and are
  re-exported by `moonpool_sim`; `buggify_knob!` lives in
  `crates/moonpool-sim/src/chaos/buggify.rs`.
- **Two-phase model.** At the start of a run every *location* is activated
  with probability 0.5 (`buggify_init(0.5)` in the runner); an active location
  then fires per call at 25% (`buggify!`) or the rate you gave. A location is
  the `file:line` of the call, so two decisions sharing a macro call share a
  fate; give each its own site.
- **Knobs are opt-in.** `buggify_knob!` only perturbs when
  `Chaos::BuggifyKnobs` is in `enable_chaos([..])`; otherwise it evaluates to
  the default, so a smoke run keeps the shipped configuration.
- Every draw comes from the shared stream, so a site is deterministic per
  `(location, seed)` and replay is exact. Adding a site shifts every later
  draw of every seed; that is expected and the reason seeds are never pinned
  as regression tests.

## Where to put a site

1. **Optional work.** Anything correct to skip: a resend, a cache refresh, a
   snapshot advertisement, a compaction pass.
2. **Error paths.** Return the error a real device would, on the success
   path: `if result.is_ok() && buggify!() { return Err(..) }`.
3. **Concurrency windows.** Insert a yield or a delay between "check" and
   "act" so a race the scheduler rarely produces becomes common.
4. **Tuning knobs.** A queue depth, a batch size, a timeout: `buggify_knob!`
   with the default and a documented range.
5. **Lifecycle.** Restart-shaped decisions the process owns (resign
   leadership, close a connection) at a low rate.

One decision per site, and consult a site only where the choice can have an
observable effect (ask "skip the resend?" only when something is pending),
otherwise the activation is spent on a no-op and the report shows a site that
"fired" without exercising anything.

## Every knob documents its floor

A knob exists only where the extreme is a **valid configuration**. Pushing
it must not make a run unwinnable: a queue that cannot hold one tick of
traffic is a permanent partition, not a knob. Write the floor and why next
to the site. Never buggify an oracle threshold (the value a check compares
against) or a schedule ceiling (iteration counts): moving those changes the
verdict, not the state explored.

## Pair every site with a reachable

```rust
if buggify_with_prob!(0.1) {
    moonpool_sim::assert_reachable!("resend skipped by buggify");
    return;
}
```

A `reachable` behind the branch proves the site fired somewhere in the
sweep without ever failing coverage on the seeds that did not draw it. Do
not use `assert_sometimes!` for that (`/using-assertions`).

## Quiet the tail

Disruptive sites should stop firing after the chaos window so the recovery
tail measures the protocol, not the injector; gate them on the same cutoff
the chaos duration uses. Sites that only shape configuration (knobs) can
stay.

Book: `book/src/part3-building/08-buggify.md`; reference: the BUGGIFY post
(<https://transactional.blog/simulation/buggify>) and
`docs/references/foundationdb/Buggify.h`.
