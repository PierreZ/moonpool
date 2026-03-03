# Prevention vs Discovery

<!-- toc -->

- Prevention: regression testing — "did we break what worked?" — guards known behavior
- Discovery: exploratory testing — "what else is broken?" — surfaces unknown failure modes
- Most teams are 100% prevention, 0% discovery
- Three enabling principles for discovery: deterministic simulation, controlled fault injection, sometimes assertions
- Sometimes assertions: evidence that interesting conditions actually occur, not just that nothing crashed
- The feedback loop: simulation infrastructure + assertions = continuous bug-finding without human test authorship
