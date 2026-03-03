# The Case for Simulation

<!-- toc -->

- The development-production gap: dev environments are clean and predictable, production is chaos
- Combinatorial explosion: even simple APIs have thousands of test scenarios, hand-writing them is structurally impossible
- Research: 2,000-machine service experiences 10+ crashes/day as normal; 80% catastrophic impact from partition failures
- Why coverage metrics lie: 100% line coverage says nothing about the state space explored
- The structural impossibility: manual tests verify what developers imagined, not what they didn't
