---
name: update-the-book
description: Keep the moonpool mdbook in step with the code - find the chapter that documents a changed API or behavior, edit it in the book's voice, and maintain SUMMARY.md, index.md and cross-references when adding or renaming a chapter. Use after changing any public API, builder method, macro, feature flag or default in a moonpool crate, when adding a new capability, or when asked to document something for readers or for AI agents (book/src/llms.md).
---

# Update the book

The book (`book/src/`) is the documentation of record, and CI builds it on
every PR. A public change without a book change ships two versions of the
truth. `book/AGENTS.md` holds the voice and structure rules and loads
automatically when you edit under `book/`; this skill is the map from a code
change to the chapter that must move.

## Which chapter

| Changed | Chapter(s) |
|---|---|
| provider traits, `Providers`, `select!` | `part2-foundations/04..07-*.md`, `appendix/06-sim-compatibility.md` |
| `Process`, groups, tags, `cluster` | `part2-foundations/09-process.md`, `part3-building/02-defining-process.md` |
| `Workload`, swarm alphabet | `part2-foundations/10-workload.md`, `part3-building/03-writing-workload.md`, `19-designing-workloads.md` |
| `SimulationBuilder` methods, iteration control, report fields | `part3-building/04-simulation-builder.md`, `05-running.md`, `appendix/03-configuration.md` |
| chaos, attrition, network/storage faults, recovery mode | `part3-building/07-chaos.md`, `09-attrition.md`, `10-network-faults.md`, `11-storage-faults.md`, `appendix/04-fault-reference.md` |
| buggify macros, knobs | `part3-building/08-buggify.md` |
| assertion macros, budgets | `part3-building/12..16-*.md`, `appendix/01-assertion-reference.md` |
| tracing capture, `Invariant`, `TraceQuery` | `part3-building/17-events-and-invariants.md` |
| metrics | `part3-building/25-metrics.md` |
| debugging, seeds, canary | `part3-building/20..23-*.md`, `part2-foundations/03-seeds.md` |
| hyper, tonic, axum integration | `part4-integration/03-wiring-a-web-service.md`, `08-hyper-stack.md` |
| exploration, frontier, recipes | `part5-building-on-top/*.md` |
| crate layout, features | `appendix/02-crate-map.md`, `part4-integration/05-production.md` |
| anything an agent would wire end to end | `llms.md` (the agent guide keeps the API anchors current) |

`grep -rn "<old name>" book/src` is the fastest way to find every mention.

## Adding or renaming a chapter

Update all three, in one change: `book/src/SUMMARY.md` (navigation and build
order), `book/src/index.md` (the sitemap with one-line summaries), and every
cross-reference in other chapters. Keep the `<!-- toc -->` marker directly
after the `# Title`.

## Voice, briefly

Senior colleague at a whiteboard: "we", direct, concrete numbers, named
concepts. Bold for emphasis, no italics, no em dashes, no semicolons for
style, no "delve", "dive deep", "Let's explore", "in this chapter we will".
Code examples go context, code, explanation, with comments that say why.
Chapters run 400 to 1200 words. Full rules: `book/AGENTS.md`.

## Verify

```bash
nix develop --command mdbook build book/
```

The wasm demo embedded in the book is built by `book/build-wasm-demo.sh` and
pins `wasm-bindgen` to the CLI version the flake ships; see
`crates/moonpool-wasm-demo/AGENTS.md` before touching it.
