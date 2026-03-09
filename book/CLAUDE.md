# Moonpool Book

## Writing Style

- **Voice**: Senior colleague explaining at a whiteboard over coffee. Use "we"/"our" naturally. Direct, warm, technically authoritative. Like a great conference talk, not a textbook.
- **Narrative flow**: Every chapter is a journey, not a list of facts. Use questions as transitions ("But what happens when...?"). Build mental models progressively. Each paragraph sets up the next.
- **Bold for emphasis**, never italics or em dashes. No semicolons for style.
- **Concrete over abstract**: Use specific numbers ("5-10 million simulation runs per night", "2,000-machine service with 10+ crashes per day"). Name real systems (FoundationDB, TigerBeetle, Antithesis).
- **Named concepts stick**: "The Sequential Luck Problem", "The Provider Pattern", "Fork at Discovery"
- **Code examples**: Context → Code → Explanation. Comments explain WHY, not WHAT.
- **Keep `<!-- toc -->`** marker after the `# Title` in every chapter (mdbook-toc preprocessor is configured).
- **Chapter length**: 400-1200 words depending on topic density. Sub-sections can be shorter (300-600 words). Appendix chapters use structured reference format (tables, lists).
- **No standard footer or sign-off** (this is a book, not blog posts).
- **References**: Link to external resources inline as part of the argument, not in a list at the end. Link to moonpool source code where helpful.

## Anti-patterns

Never use: "delve into", "dive deep", "in the world of", "it's worth noting", "Let's explore", "In this chapter we will". No meta-commentary about the chapter itself. No filler sentences that sound insightful but say nothing ("The pattern is clear", "Understanding this changes everything").

## Structure

The book lives in `book/src/` with chapters organized into:
- **Foreword**: Project status
- **Part I (Why)**: Motivation and philosophy
- **Part II (Foundations)**: Determinism, providers, process/workload
- **Part III (Building)**: First simulation, chaos, assertions, invariants, debugging
- **Part IV (Integration)**: Standalone moonpool-sim, mock boundaries, axum example
- **Part V (Networking)**: Transport layer, peers, RPC, #[service] macro
- **Part VI (Building on Top)**: Virtual actors, multiverse exploration
- **Appendix**: Reference tables, glossary, configuration

## Commands

```bash
# Build the book
nix develop --command mdbook build book/

# Serve locally with hot reload
nix develop --command mdbook serve book/
```

## Context Sources

The book draws from 22 context sources in `~/workspace/claude/simulation-context-extract/`:
- 6 talk recaps (Will Wilson, Lawrie Green, Pierre Zemb)
- 4 Antithesis blog post recaps (Zelda, Castlevania, Gradius, Metroid)
- 7 own blog post recaps (Pierre Zemb)
- 5 research gist recaps (BUGGIFY, Oxide mocks, assertions, OSS instrumentation)
- 1 master synthesis (SUMMARY.md)
