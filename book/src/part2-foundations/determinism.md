# Determinism as a Foundation

<!-- toc -->

- Why determinism is non-negotiable: without it, you can't reproduce, can't debug, can't reason about failures
- Two sources of non-determinism in typical Rust: thread scheduling and real I/O
- Moonpool's answer: eliminate both — single-core execution + provider abstraction
