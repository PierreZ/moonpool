# Prevention vs Discovery

<!-- toc -->

Most teams treat testing as one activity. Write tests, run tests, read the results. But two different activities hide under that one word, and mixing them up is one of the most expensive mistakes in software engineering.

## Prevention: guarding what you know

The first kind of testing is **prevention**. You ship a feature. A user reports a bug. You fix it and write a test that would have caught it. The next time someone touches that code, the test fails before the bug can return.

This is regression testing. It is valuable, well understood and nearly universal. CI pipelines run thousands of these tests on every commit. The mental model is defensive: we build a wall around known-good behavior, one brick per bug report.

Prevention answers one question: **did we break what already worked?**

## Discovery: finding what you do not know

The second kind of testing is **discovery**. Discovery does not guard known behavior. It searches for failure modes nobody has found yet. It asks: **what else is broken?**

Prevention tests encode scenarios a developer imagined. Discovery tests generate scenarios no developer imagined. Prevention is a wall. Discovery is a search party sent into unmapped territory.

Most teams have a portfolio that is 100% prevention and 0% discovery. Every test answers a requirement or a bug report. No test was written to find bugs the team does not know about. This is not a criticism. Until recently, discovery at scale was available only to teams that could spend years on infrastructure. The tools have caught up.

## Three principles that enable discovery

What does it take to move from prevention to discovery? Three principles, each built on the one before.

**Deterministic simulation.** Discovery generates huge numbers of scenarios and must replay any that fail. If a bug needs a specific order of network events, we need that exact order again. Determinism makes every execution reproducible from one seed. A failing seed is a bug report anyone on the team can replay. Without determinism, discovery produces ghosts.

**Controlled fault injection.** Discovery must exercise the paths production hits and development does not: partitions, disk corruption, reordering, clock skew, a crash in the middle of recovery. These are the normal operating conditions of a system at scale. Controlled injection means the simulator decides when and how faults occur, from the same seed, so every fault is reproducible and tunable.

**Sometimes assertions.** This is the subtle one. Prevention uses assertions that say "this must always be true." Discovery needs a second kind: "this should **sometimes** be true." A sometimes assertion on a retry path does not check that retries work every time. It checks that the simulation reached the retry path at all. Without it, you can run a million simulations and never know whether they explored the interesting parts of the state space or repeated the happy path a million times.

In moonpool these are `assert_always!` and `assert_sometimes!`, two of the 15 macros in [Assertions: Finding Bugs](../part3-building/12-assertions.md).

## The feedback loop

Put the three principles together. Simulation generates scenarios. Fault injection pushes them into failure territory. Always assertions catch violations. Sometimes assertions tell us whether we explored deeply enough.

When a sometimes assertion never fires, the simulation is not reaching some region of the state space. That is a signal to change something: enable another fault family, change the workload, add concurrent operations. When every sometimes assertion fires and every always assertion holds, we have **evidence** (not proof) that the system handles the failure modes we care about.

Moonpool turns this loop into the default stop condition. `IterationControl::UntilCoverageStable` runs seed after seed until every sometimes assertion has fired and code coverage has stopped growing for `plateau_seeds` seeds in a row. See [Running and Observing](../part3-building/05-running.md).

The developer's job changes. Instead of writing one test per scenario, you define correctness properties and read the results. The infrastructure does the exploration.

Lawrie Green of Antithesis described assertions as memos to **two** audiences. They tell the computer what to check. They tell the developer what they believe about their system. When an assertion fails on correct code, the developer's mental model was wrong. That is a finding too.

## Guidance: a third job for assertions

Discovery has one more use for assertions. When a simulation reaches a state it has never seen before (a sometimes assertion fires for the first time, a numeric watermark improves), that moment is worth more exploration. Moonpool's explorer forks the process at exactly that point and continues from the new state with fresh randomness. The assertion is no longer only a check. It is a signal that guides the search. See [Invariants vs Discovery vs Guidance](../part3-building/13-assertion-concepts.md) and [Multiverse Exploration](../part5-building-on-top/01-exploration.md).

## A spectrum, not a binary

Prevention and discovery do not compete. Every team needs prevention. Adding even a small amount of discovery changes what you find.

You do not have to jump from zero to full deterministic simulation. Random delays in an integration test are discovery. Property-based tests with random inputs are discovery. One sometimes assertion in an existing simulation is discovery.

The question is not whether to do discovery testing. The question is how far along the spectrum to go. Each step finds bugs that prevention, by construction, never will.
