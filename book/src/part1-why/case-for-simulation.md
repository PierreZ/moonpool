# The Case for Simulation

<!-- toc -->

You deploy on a Friday afternoon. The tests are green. Code review was thorough. You even ran the integration suite twice. You go home.

At 2am, your phone lights up. A network partition isolated two nodes for eleven seconds. During recovery, a message arrived out of order. A retry collided with a timeout. The system entered a state that nobody on your team imagined was reachable. Data was lost.

The code was correct for the world your tests described. It was not correct for the world production delivered.

## The gap

Development environments are clean. The network is localhost. Disks never fail. Clocks agree. Messages arrive in order, exactly once. We know this is fictional, yet our test environments faithfully reproduce the fiction. We test against a world that does not exist, then express surprise when the real world finds the bugs we missed.

Production is a different animal. A [study of failures across Google, Microsoft, and Yahoo services](https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-yuan.pdf) found that a 2,000-machine service experiences more than 10 machine crashes per day as **normal operations**. Network partitions are not rare events reserved for conference war stories. Research on [network partition failures in cloud services](https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-alquraan.pdf) showed that 80% of catastrophic failures in distributed systems are caused by partition-related bugs, and 27% of those result in data loss. These are not exotic edge cases. They are Tuesday.

The gap between development and production is not a minor oversight we can close by writing more careful tests. It is structural.

## The combinatorial problem

Consider a modest distributed system: three nodes, a leader election protocol, and replicated state. Now list the things that can go wrong. Any node can crash and restart. The network between any pair of nodes can partition. Messages can be delayed, reordered, or duplicated. Disk writes can be torn or lost. Clocks can drift.

A single test scenario might be: "Node B crashes during a leader election while Node A has an in-flight write and the network between A and C drops for two seconds." That is one scenario. How many are there?

Even with coarse-grained modeling, a three-node cluster with five failure types and ten time steps produces thousands of distinct failure histories. A five-node cluster with realistic failure granularity produces millions. And that is before considering application-level state: what data was in flight, which transactions were uncommitted, which clients were retrying.

Consider a simple e-commerce API as an example. Six variable dimensions (user types, payment methods, delivery options, promotions, inventory status, currencies) require 648 unique test combinations for basic coverage. Adding one option to each dimension pushes it past 4,000. A real system has hundreds of dimensions. In one case we experienced at Clever Cloud, a 300-line feature required a 10,000-line test PR to maintain combinatorial coverage.

This is not a tooling problem. No test framework makes writing 10,000 tests sustainable. The combinatorial space of a distributed system grows faster than any team can write tests for it.

## Why coverage metrics lie

You might look at your coverage report and feel reassured. 85% line coverage. 70% branch coverage. These numbers measure how much **code** your tests execute. They say nothing about how much **state space** your tests explore.

A distributed system can execute the same lines of code in thousands of different orderings with thousands of different timing relationships. Line coverage treats all of those as identical. Branch coverage is slightly better but still blind to interleaving. You can have 100% branch coverage and never once test what happens when a leader election overlaps with a network partition during a compaction.

Will Wilson put it precisely: the very reason tests are needed (humans cannot enumerate all control flow paths) is exactly what makes it impossible for humans to write comprehensive tests. This is not a failure of discipline. It is a logical impossibility. Manual tests verify what developers imagined. They cannot verify what developers did not imagine. And bugs, by definition, live in the places nobody imagined.

## The structural impossibility

Here is the argument in its sharpest form.

Distributed systems fail in ways that depend on the ordering of concurrent events. The number of possible orderings grows combinatorially with system size. Humans write tests based on scenarios they can imagine. The scenarios that cause bugs are, almost by definition, the ones nobody imagined. Therefore, comprehensive manual testing of a distributed system is not merely difficult. It is structurally impossible.

This does not mean we should give up on testing. It means we need a fundamentally different approach. Instead of writing individual tests by hand, we need to generate tests automatically. Instead of testing against a clean, predictable environment, we need to test against one that is **worse** than production. Instead of hoping we covered the important cases, we need infrastructure that systematically explores the space of all possible failures.

That is what simulation gives us. Not better tests. A different kind of testing entirely.
