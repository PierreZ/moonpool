# Chaos Testing vs Simulation

<!-- toc -->

- Chaos engineering (Chaos Monkey style): black-box, in production, non-deterministic, reactive
- Simulation: white-box, at development time, deterministic, proactive
- Key insight: simulation subsumes chaos — you get fault injection WITH reproducibility, speed, and exhaustiveness
- Chaos engineering finds symptoms; simulation finds root causes
- Simulation runs millions of scenarios in minutes; chaos engineering runs one scenario in production
- They're complementary: simulation for development, chaos for production validation
- Moonpool's approach: chaos injection is a tool WITHIN the simulation, not an alternative to it
