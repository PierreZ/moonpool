# The Case for Simulation

<!-- toc -->

You deploy on a Friday afternoon. The tests are green. Code review was thorough. You ran the integration suite twice. You go home.

At 2am your phone lights up. A network partition isolated two nodes for eleven seconds. During recovery, a message arrived out of order. A retry collided with a timeout. The system entered a state nobody on the team had imagined. Data was lost.

The code was correct for the world your tests described. It was not correct for the world production delivered.

## The gap

Development environments are clean. The network is localhost. Disks never fail. Clocks agree. Messages arrive in order, exactly once. We know this is fiction. Our test environments reproduce the fiction anyway. We test against a world that does not exist, then we are surprised when the real world finds the bugs we missed.

Production is different. A [study of 198 failures in Cassandra, HBase, HDFS, MapReduce and Redis](https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-yuan.pdf) found that 74% were deterministic. Most could be reproduced on three nodes or fewer. 77% could have been caught by a unit test that checked the error path. The bugs were not exotic. They were ordinary mistakes in error-handling code that nobody tested. A later [study of network partition failures](https://www.usenix.org/system/files/osdi18-alquraan.pdf) showed that 80% of catastrophic failures in distributed systems involve a partition, and 27% of those lose data. These are not edge cases. They are Tuesday.

The gap between development and production is not a small oversight. More careful tests do not close it. It is structural.

## The combinatorial problem

Take a modest system: three nodes, a leader election, replicated state. Now list what can go wrong. Any node can crash and restart. The network between any pair of nodes can partition. Messages can be delayed, reordered or duplicated. Disk writes can be torn or lost. Clocks can drift.

One scenario looks like this: "Node B crashes during an election while Node A has a write in flight and the link between A and C drops for two seconds." That is one scenario. How many are there?

With coarse modeling, three nodes, five failure types and ten time steps give thousands of distinct failure histories. Five nodes with realistic granularity give millions. That is before application state: which data was in flight, which transactions were uncommitted, which clients were retrying.

The same problem appears in ordinary services. An e-commerce API with six dimensions (user type, payment method, delivery option, promotion, inventory status, currency) needs 648 combinations for basic coverage. One more option per dimension pushes it past 4,000. A real system has hundreds of dimensions. On one real project, a 300-line feature needed a 10,000-line test PR to keep combinatorial coverage.

This is not a tooling problem. No test framework makes 10,000 hand-written tests sustainable. The state space of a distributed system grows faster than any team can write tests for it.

## Why coverage numbers lie

A coverage report says 85% line coverage and 70% branch coverage. These numbers measure how much **code** the tests execute. They say nothing about how much **state space** the tests explore.

A distributed system can run the same lines in thousands of orderings with thousands of timing relationships. Line coverage treats all of them as one. Branch coverage is a little better but still blind to interleaving. You can have 100% branch coverage and never test what happens when an election overlaps with a partition during a compaction.

Will Wilson put it precisely: humans cannot enumerate all control-flow paths, which is why we need tests. The same limit makes it impossible for humans to write complete tests. This is not a failure of discipline. Manual tests verify what developers imagined. They cannot verify what developers did not imagine. Bugs live in the places nobody imagined.

## The structural argument

Here is the argument in its shortest form.

1. Distributed systems fail in ways that depend on the order of concurrent events.
2. The number of orderings grows combinatorially with system size.
3. Humans write tests for scenarios they can imagine.
4. The scenarios that cause bugs are the ones nobody imagined.

So complete manual testing of a distributed system is not hard. It is impossible.

This does not mean we stop testing. It means we need a different kind of test. Instead of writing scenarios by hand, we generate them. Instead of a clean environment, we test in one that is **worse** than production. Instead of hoping we covered the important cases, we build infrastructure that explores the failure space for us.

That is what simulation gives us. Not better tests. A different kind of testing.

## What simulation needs

Three properties make this kind of testing work. The rest of Part I and all of Part II come back to them.

**Determinism.** The whole system runs on one thread with one seeded random source. The same seed gives the same execution, so a failure is a number you can replay. See [Determinism as a Foundation](../part2-foundations/01-determinism.md).

**Control of the environment.** Time, tasks, network, disk and randomness go through traits with two implementations: the real one for production, the simulated one for tests. The simulated one is where faults are injected. See [The Provider Pattern](../part2-foundations/04-provider-pattern.md).

**Properties, not expectations.** Tests state what must always hold and what must sometimes happen. The infrastructure generates the scenarios and checks the properties. See [Prevention vs Discovery](./02-prevention-vs-discovery.md).
