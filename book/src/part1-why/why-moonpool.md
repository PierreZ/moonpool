# Why Moonpool Exists

<!-- toc -->

- Personal story: the motivation, the problem being solved
- The synthesis: FDB's networking simulation + TigerBeetle's storage fault patterns + Orleans's virtual actor model
- What moonpool tries to be: a Rust framework for building systems that are tested by simulation from day one
- Where it sits on the adoption spectrum: library-level simulation (no hypervisor), trait-based DI, single-crate integration
- What makes it different: fork-based multiverse exploration, Antithesis-style assertion suite, integrated actor model
- Teaser: exploration can find bugs that require sequences of lucky events — covered in Part V
