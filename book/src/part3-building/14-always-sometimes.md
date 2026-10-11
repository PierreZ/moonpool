# Always and Sometimes

<!-- toc -->

Five boolean macros are the base of the assertion system. Each takes a message that identifies the site across seeds and across forked timelines. Three of them accept an optional block of details for the failure line.

## assert_always!

The workhorse. The condition must hold every time the line runs.

```rust
// From the metered key/value example: a gauge that went negative is a bug
// in the admission path, not in the metric.
assert_always!(
    metrics.in_flight.get() >= 0.0,
    "in-flight gauge never goes negative"
);
```

When the condition is false, the slot's fail count goes up, the seed is marked failed, and the run continues. The first failure of a slot prints `[ASSERTION FAILED] <message> (kind=Always)` to stderr. Later failures of the same slot only count.

A details block turns every failure into a line with the seed and the values you name:

```rust
assert_always!(
    executed <= self.permitted,
    "a balanced job never runs more often than its permissions allow",
    { "id" => self.id, "executed" => executed, "permitted" => self.permitted }
);
```

```text
[ASSERTION FAILED] a balanced job never runs more often than its permissions allow (seed=9182) | id=4, executed=3, permitted=2
```

Every value must implement `Display`. The details are printed, never stored: the table keeps counts, not strings.

`assert_always!` also fails when it is **never reached** in the whole run. The report says `assert_always!('...') was never reached`. The reason is false confidence: an invariant on a recovery path that recovery never enters has checked nothing. Fix the simulation so the path runs, or switch to the next macro.

## assert_always_or_unreachable!

The same contract, minus the reach requirement.

```rust
assert_always_or_unreachable!(
    balance >= 0,
    "balance stays non-negative after a withdrawal",
    { "balance" => balance }
);
```

If no seed ever withdraws, the slot stays empty and the report says nothing. If a withdrawal does happen and the balance goes negative, that is a violation like any other. The report prints its kind as `always?`.

Use it for paths that a given chaos configuration may legitimately skip. Reserve plain `assert_always!` for the paths you insist on testing. Mixing them up produces a flood of "never reached" violations the first time you narrow a swarm mask.

## assert_sometimes!

The condition must be true at least once across all seeds. Any single evaluation may be false.

```rust
// From the metered key/value workload's check(): did the time series
// actually record latency observations in at least one seed?
assert_sometimes!(
    series.keys().any(|k| k.starts_with("kv_request_latency")),
    "latency observations are recorded as a time series"
);
```

Three things happen on the first pass of a `sometimes` slot:

1. The pass count goes from 0 to 1, which clears the coverage violation.
2. The slot now counts as reached for `UntilCoverageStable`.
3. The discovery hook fires once, and the explorer, if present, forks from this RNG position.

If the slot is evaluated and never passes, the report lists `assert_sometimes!('...') has 0% success rate (N checks)` under Coverage Gaps. A `sometimes` that is never evaluated at all is not listed, because the table never saw it.

`assert_sometimes!` has no details block. Its message is `&$message`, so a `&str` or a `String` both work, but remember that each distinct string is a separate slot.

A practical pattern from the dungeon example: name the thing, not the condition.

```rust
if self.game.player_tile() == Tile::Treasure {
    assert_sometimes!(true, "near treasure");
}
```

`assert_sometimes!(true, ...)` inside an `if` is the same as `assert_reachable!`, and sometimes reads better when the label describes a game state rather than a code path.

## assert_reachable!

A code path that must run at least once. No condition.

```rust
// From the runner's reboot code: both reboot shapes must be exercised.
match kind {
    RebootKind::Graceful => {
        assert_reachable!("reboot: graceful path");
        // ...
    }
    RebootKind::Crash | RebootKind::CrashAndWipe => {
        assert_reachable!("reboot: crash path");
        // ...
    }
}
```

The slot is a `Reachable` kind with the condition fixed to `true`. Its first reach is a `SometimesPass` discovery, and an unreached site is reported as `assert_reachable!('...') was never reached`. Moonpool's own runner uses it to prove that its fault paths ran: `hint: the harness's veto refuses a reboot`, `reboot: target already dead, skipped`, and so on. Shipped code uses the inert twin `moonpool_assertions::reachable!` for the same purpose.

## assert_unreachable!

The inverse: a path that must never run.

```rust
fn handle(&mut self, msg: Message) -> Result<(), Error> {
    match msg.kind {
        MessageKind::Request => { /* ... */ }
        MessageKind::Response => { /* ... */ }
        MessageKind::Unknown => {
            assert_unreachable!("received an unknown message kind", { "from" => msg.from });
            return Err(Error::UnknownMessage);
        }
    }
    Ok(())
}
```

Reaching the line is the violation. The slot records a pass per reach (the count of "times reached"), the seed is marked failed, and the first reach prints `[UNREACHABLE REACHED] <message>`. The report says `assert_unreachable!('...') was reached N times`.

The code after the macro **still runs**. Moonpool does not throw. That is deliberate: the run goes on to show what the system does after the impossible happened.

## Choosing

| You want to say | Macro |
|-----------------|-------|
| Always true, and this path must be tested | `assert_always!` |
| Always true, but the path may stay cold | `assert_always_or_unreachable!` |
| True at least once | `assert_sometimes!` |
| This path runs at least once | `assert_reachable!` |
| This path never runs | `assert_unreachable!` |

Start with invariants on the core properties. Then add `sometimes` and `reachable` on the error branches, the retries and the recovery code. Those are the lines that keep `UntilCoverageStable` running until chaos has actually visited them, and the lines the explorer forks from.
