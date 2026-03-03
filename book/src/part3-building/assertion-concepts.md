# Invariants vs Discovery vs Guidance

<!-- toc -->

- Three categories of assertions:
  - **Invariants** (Always): properties that must hold on every check — if violated, there's a bug
  - **Discovery** (Sometimes/Reachable): properties that must occur at least once — ensures the simulation actually exercises interesting paths
  - **Guidance** (Numeric/Compound): properties that steer exploration — the system actively tries to satisfy them
- Why the taxonomy matters: different assertion types serve different purposes and interact differently with the explorer
- Invariant assertions validate; discovery assertions provide coverage evidence; guidance assertions amplify exploration
