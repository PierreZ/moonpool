# Multi-Seed Exploration

<!-- toc -->

A single root seed reaches a particular region of the state space. No amount of continuation can explore regions that the root timeline's early execution path never touches: if the first 100 RNG calls establish a specific dungeon layout or network topology, every recipe extension shares that foundation. Variations on a theme, not different themes.

The builder's ordinary multi-seed loop provides the themes. Exploration makes each additional theme cheap.

## Novelty Is Cumulative

The discovery latches in the shared assertion region — the CAS guards that make each discovery fire exactly once — are deliberately **not** reset between seeds, and neither is the sancov history map. Novelty is a property of the whole run, not of one seed:

- If seed 1 already discovered "key found L1", seed 2's timelines that find the same key journal nothing.
- If seed 1's exploration drove the "dungeon level reached" watermark to 5, seed 2 only journals a discovery by beating 5.
- Assertion pass/fail counts also accumulate, so the final report and contract validation reflect every seed.

The consequence is a naturally *progressive bar*: each seed only earns exploration budget for behavior no earlier timeline exhibited.

## Barren Seeds Die After One Run

`max_runs_per_seed` is a ceiling, not a quota. Each seed's exploration starts from its root run's journal; a root run that discovers nothing globally new produces no expansion, no exemplars, and therefore no continuations — the seed finishes after a single timeline:

```text
━━━ Seeds ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  #1  seed=16797655684637781087  1,500 timelines   ← did the deep work
  #2  seed=6458319710110286448       1 timelines   ← nothing new here
  #3  seed=10665710964006250804      1 timelines   ← nothing new here
```

This replaces the previous design's warm-start heuristics (`warm_min_timelines`, coverage-yield ramp-downs): there is no machinery deciding how much to invest in an already-explored seed, because an unproductive seed simply generates no work. Under `UntilCoverageStable`, cheap barren seeds mean the run samples many more root themes per unit of wall-clock time while the plateau detector watches the same cumulative coverage signals as always.

## The Complete Picture

**The problem**: bugs that require sequences of unlikely events are exponentially hard to find with random seeds.

**The frontier controller**: deterministic replay turns executions into recipes; discoveries (CAS-latched, exactly-once) turn productive runs into a small number of follow-up jobs; barren branches die. No energy accounting.

**Bounded workers**: `fork()` executes jobs cheaply via copy-on-write, but the process tree is never the exploration tree — one controller plus a fixed worker pool, on Linux and macOS alike.

**Exemplars and continuations**: a few retained recipes per semantic state, scheduled from the under-explored, depth-weighted frontier, keep converting run budget into deeper progress.

**Multi-seed exploration**: cumulative novelty makes extra seeds nearly free, so breadth across themes composes with depth within them.

The developer's job is to write good assertions — they are simultaneously the correctness spec and the exploration map. The controller handles the rest, and every timeline it ever visits can be replayed from a one-line recipe.
