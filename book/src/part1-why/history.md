# A Brief History

<!-- toc -->

- FoundationDB's radical bet: single-threaded pseudo-concurrency, simulated network, BUGGIFY chaos injection
  - 5-10 million simulation runs per night, trillions of CPU-hours tested
  - Custom Flow language compiled to callbacks for determinism
  - The sinkhole cluster: real hardware with programmable power switches to validate simulation findings
- Antithesis generalizes it: deterministic hypervisor makes any software simulatable
  - No more custom languages or single-threaded rewrites
  - SDK with declarative assertions that guide exploration
  - Customers find bugs within 2-3 weeks in systems with existing test suites
- The adoption spectrum: from elite teams (FDB, 2-year investment) → commercial platform (Antithesis) → small teams (5-person Clever Cloud, 30min sim = 24h chaos)
- The philosophy that unites them: make all execution deterministic, inject faults, explore systematically
