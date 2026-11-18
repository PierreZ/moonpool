# moonpool-tla

Model-based trace checking for the moonpool framework, with zero
dependencies: is this run of the real code a behaviour that a TLA+ spec
allows?

A run records its events in order in a `Trace`. `TraceCheck` writes the
trace as a TLA+ module, runs TLC on a hand-written trace spec that extends
the real spec and follows the events one by one, and returns a `Verdict`:

| Verdict | Meaning |
|---------|---------|
| `Accepted` | A behaviour of the spec matched every event, and every invariant held |
| `Rejected(rejection)` | No behaviour matches: `rejection.index` is the first event that does not match, `rejection.state` the model state before it |
| `InvariantViolated { .. }` | A behaviour matched a prefix and broke an invariant of the spec |

```rust,no_run
use moonpool_tla::{Trace, TraceCheck, Value, Verdict};

let mut trace = Trace::new();
trace.define("TraceRM", Value::str_set(["r1", "r2"]));
trace.event("rm_prepare", [("rm", Value::from("r1"))]);
trace.event("tm_commit", Vec::<(String, Value)>::new());

// specs/ holds TwoPhase.tla, TwoPhaseTrace.tla and TwoPhaseTrace.cfg.
let verdict = TraceCheck::new("specs", "TwoPhaseTrace").check(&trace)?;
assert!(matches!(verdict, Verdict::Rejected(_)));
# Ok::<(), moonpool_tla::CheckError>(())
```

The method is the one of Cirstea, Kuppe, Loillier and Merz, "Validating
Traces of Distributed Programs Against TLA+ Specifications" (2024).

- `moonpool-sim` builds a `Trace` from its captured timeline
  (`moonpool_sim::observability::tla::trace_from_timeline`); a sans-I/O
  harness records events itself.
- Running TLC needs the `tlc` launcher (nixpkgs `tlaplus`, installed by the
  moonpool dev shell) or the path in `MOONPOOL_TLC`.
- The book chapter "Checking Runs Against a TLA+ Spec" explains the trace
  spec, the verdicts and the controls.
