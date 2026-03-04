# Prevention vs Discovery

<!-- toc -->

Most teams think about testing as one thing. Write tests, run tests, check the results. But there are actually two fundamentally different activities hiding under that single word, and conflating them is one of the most consequential mistakes in software engineering.

## Prevention: guarding what you know

The first kind of testing is **prevention**. You ship a feature. A user reports a bug. You fix the bug, then write a test that would have caught it. Next time someone modifies that code, the test fails before the bug can return.

This is regression testing. It is valuable, well-understood, and nearly universal. CI pipelines run thousands of these tests on every commit. The mental model is defensive: we are building a wall around known-good behavior, brick by brick. Each bug report adds a brick.

Prevention testing answers one question: **did we break what already worked?**

## Discovery: finding what you do not know

The second kind of testing is **discovery**. Rather than guarding known behavior, discovery testing actively searches for unknown failure modes. It asks: **what else is broken that we have not found yet?**

Prevention tests encode specific scenarios a developer imagined. Discovery testing generates scenarios no developer imagined. Prevention is a wall. Discovery is a search party sent into unmapped territory.

Most teams have a testing portfolio that is 100% prevention and 0% discovery. Every test was written in response to a specific requirement or a specific bug. No test was written to find bugs the team does not yet know about. This is not a criticism of those teams. Until recently, discovery testing at scale was only available to a handful of organizations willing to invest years of infrastructure work. But the tools are catching up.

## Three principles that enable discovery

What does it take to shift from prevention to discovery? Three enabling principles, each building on the last.

**Deterministic simulation.** Discovery testing must generate enormous numbers of scenarios and be able to reproduce any that fail. If a bug depends on a specific ordering of network events, we need to replay that exact ordering. Determinism makes every execution reproducible from a single seed value. A failing seed is a bug report that anyone on the team can replay, inspect, and fix. Without determinism, discovery testing produces irreproducible ghosts.

**Controlled fault injection.** Discovery testing must exercise failure paths that production encounters but development environments do not. Network partitions, disk corruption, message reordering, clock skew, process crashes during recovery. These are not exotic scenarios. They are the normal operating conditions of any distributed system at scale. Controlled injection means we decide when and how faults occur, driven by a seeded random number generator so they are reproducible and tunable.

**Sometimes assertions.** This is the subtle one. Prevention tests use assertions that say "this must always be true." Discovery testing needs a second kind: assertions that say "this should **sometimes** be true." A sometimes assertion on a timeout retry path does not check that retries work every time. It checks that our simulation actually exercised the retry path at all. Without sometimes assertions, you can run a million simulations and never know whether they explored the interesting parts of the state space or just repeated the happy path a million times.

## The feedback loop

Put the three principles together and something powerful emerges. Simulation generates scenarios. Fault injection pushes those scenarios into failure territory. Always assertions catch violations. Sometimes assertions tell us whether we explored deeply enough.

When a sometimes assertion never fires, it means our simulation is not reaching some region of the state space. That is a signal to adjust: inject different faults, change the workload, add more concurrent operations. When all sometimes assertions are firing and all always assertions hold, we have **evidence** (not proof, but strong evidence) that our system handles the failure modes we care about.

This is the discovery feedback loop. It runs continuously. It finds bugs that no developer anticipated. And critically, it does not require writing new tests for each new scenario. The infrastructure does the exploration. The developer's job shifts from writing tests to defining correctness properties and interpreting results.

Lawrie Green from Antithesis captured this duality perfectly: assertions are memos to **two** audiences. They tell the computer what to check during exploration. And they tell the developer what they believe about their system. When an assertion fails on correct code, the developer's mental model was wrong. That is itself a critical finding.

## The spectrum, not the binary

Prevention and discovery are not competing approaches. Every team needs prevention testing. But adding even a small amount of discovery testing changes the game fundamentally.

You do not need to go from zero to full deterministic simulation overnight. Injecting random delays into your existing integration tests is discovery testing. Property-based tests with randomized inputs are discovery testing. Adding sometimes assertions to your simulation suite is discovery testing.

The question is not whether you should do discovery testing. The question is how far along the spectrum you want to go. Every step reveals bugs that prevention testing, by construction, will never find.
