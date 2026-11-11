# website/site

The moonpool website (#414 (website makeover), plan approved by Pierre on
2026-11-11). One Astro app, one style for every page, after the paros site
(`PierreZ/paros`, `website/site/` and `website/shared/`). The Sim Book is in it
as the Guide.

Build: `scripts/build-wasm-demo.sh` (from any folder, it stages the wasm demo in
`public/wasm-demo/`), then `npm ci && npm run build` here (output in `dist/`).
Preview: `npm run dev`. Unit tests: `npm test`. Run all of it inside
`nix develop` (the flake has `nodejs`).

## What the site is for

The site presents moonpool as an engineering tool. The book is its manual.

| Path | Source | Job |
|---|---|---|
| `/` | `src/pages/index.astro`, `public/home.js` | sells the tool: a hero with a live run, what you get, a test, your code and the providers, the techniques, used by paros |
| `/techniques/` | `src/pages/techniques/`, `src/lib/techniques.mjs`, `src/layouts/Technique.astro` | one page per technique: the problem, the technique, the live run, the code, the Guide chapters |
| `/playground/` | `src/pages/playground/index.astro`, `public/playground.js` | the full config in the address (`playConfig` / `playQuery` of `public/run-model.js`), Run, Hunt (seeds one after the other in the worker, stoppable), Replay, the report |
| `/extras/` | `src/pages/extras/`, `src/lib/extras.mjs`, `src/layouts/Extra.astro` | one page per crate, in depth: its module docs, its byte layouts and its code, read with `src/lib/rustdoc.mjs` (`moduleDoc`, `docSection`, `unlink`, `encodeLayout`, `constValue`, `itemDoc`, `fieldDocs`) and drawn by `src/components/ByteLayout.astro` |
| `/crates/` | `src/pages/crates/index.astro`, `src/lib/crates.mjs` | one entry per crate of the README table: what it is for, when to depend on it, docs.rs, its chapter |
| `/guide/` | `src/pages/guide/index.astro` | `book/src/welcome.md`, then the map of `book/src/index.md` |
| `/guide/quick-start/` | `src/pages/guide/quick-start/index.astro` | install, the first test, run it, read the report, replay a seed |
| `/<part>/<chapter>.html`, `/llms.html` | `src/pages/[...slug].astro` | each chapter of `book/src/SUMMARY.md`, at its mdbook address |

Add a
section to the top bar (`src/nav.ts`) in the issue that adds its page.

## The Guide

- The chapters stay in `book/src/`. `src/lib/book.mjs` reads `SUMMARY.md` (the
  one source of the order: sidebar and pager), makes each relative link
  absolute (`.md` becomes `.html`), takes the `# Title` off for the title
  block, and puts the table of contents where the chapter has `<!-- toc -->`.
- A link to a `.md` file that is not in `SUMMARY.md` fails the build.
  `scripts/redirects.mjs` then checks each link inside `dist/`.
- The URLs of the book never change: the crate READMEs, `Cargo.toml` and
  docs.rs link to them. `llms.html` keeps its address and its content (the
  paros agents read it). `welcome.html`, `print.html` and `toc.html` redirect.
- The search is Pagefind: `npm run build` indexes the elements with
  `data-pagefind-body` (the chapter text) into `dist/pagefind/`.
- Only plain Markdown in a chapter. The voice of the chapters is in
  `book/AGENTS.md`.

## Facts from the code

A page never holds a copy of code or of a figure that can go stale.
`src/lib/source.mjs` reads them at build time:

- the crate-level doc test of `moonpool` (`crates/moonpool/src/lib.rs`): the
  test of the home page and the quick start;
- the crate table of the root `README.md`: the crates page;
- the figures of the home page: the `Providers` types, the `Chaos` variants,
  the `assert_*` macros;
- the workspace version: the dependency lines.

`src/lib/lib.test.mjs` checks each reader against the real files.

## Live runs

The rules are in #414 (website makeover), "Animation rules". Each run is a real
moonpool run compiled to wasm; the page draws what the run recorded, and no
simulation logic is in JavaScript. A page shows a recorded run as JSON first
(written by a Rust test, never by hand), so it is complete before the wasm
loads and when it does not load. The address holds the seed (`?seed=256`).
With "reduce motion" on, nothing plays alone.

The engine is `crates/moonpool-wasm-demo`: `run(config)` runs one seed of the
small system of the site (a replicated register) and returns the run as JSON
(`Run` in its `src/engine.rs`). The page side:

- `public/engine.js` and `public/engine-worker.js`: the wasm in a Web Worker,
  made when a run is near the screen or at the first click.
- `public/player.js`: the one player (lanes, messages, faults, down spans,
  checks, controls, the list of events); `public/run-model.js` reads a run
  for it and `src/lib/player.test.mjs` tests that part.
- `tests/site_runs.rs` writes the recorded runs of each page in `src/runs/`
  (one file per page) and fails when a file was stale: commit it.
- `scripts/check-site-wasm.sh` (repository root) proves that one config gives
  the same JSON natively and on wasm32. CI runs it.

Pages: the home hero (`public/home.js`), `/embed/register.html` (the run
alone in the frame of the chapter "Simulation in the Browser"), and one script
per technique page:

| Page | Script | What it adds under the player |
|---|---|---|
| `/techniques/determinism/` | `public/compare.js` | a second run of a seed, side by side |
| `/techniques/same-code/` | `public/same-code.js` | the server's lines and the events of each; a search for a seed that strikes at a hint |
| `/techniques/network/` | `public/network.js` | a switch per network fault family (`NETWORK_FAULTS` of the engine) and the faults of each kind |
| `/techniques/storage/` | `public/storage.js` | the log of each server as a row of sectors (`log_written`, `log_synced`, `storage_crash_sectors`) |
| `/techniques/reboots/` | `public/reboots.js` | each reboot: kind, down, back, the version on its disk |
| `/techniques/swarm/` | `public/swarm.js` | eight swarm seeds as rows, a dot per family each drew (`Run.families`, from the `chaos_families` event); a row opens its run |
| `/techniques/assertions/` | `public/assertions.js` | the register's checks (`CHECKS`) turning green seed after seed, a coverage bar; the stop rule is the engine's `coverage` export |
| `/techniques/exploration/` | `public/exploration.js` | the recorded tree of an exploration (`tests/exploration_tree.rs`, the explorer forks so it never runs in a browser); the bug timeline replays from its `recipe` |
| `/techniques/debugging/` | `public/debugging.js` | the report of a failing seed, its replay, and the walk back from the broken invariant (`walkBack` in `public/run-model.js`) |

A technique page reads its code samples and names from the files
(`excerpt` (with `after` to keep lines past the end marker), `lineWith`, `constList`, `constNames`, `constStr`, `hintLabels` in
`src/lib/source.mjs`), so a sample never goes stale: a marker that is gone
fails the build. Each page links to its Guide chapters, and each chapter links
back to the page.

## One style

- Every page uses `src/layouts/Base.astro` (top bar, footer). A reading page
  uses `src/layouts/Doc.astro`, a page of the Guide `src/layouts/Guide.astro`
  (sidebar from `SUMMARY.md`, search, pager).
- Colours, fonts and components come from `website/shared/`.
  `website/shared/tokens.css` is the only file with a colour. `base.css`,
  `theme.js` and the fonts are a copy of the paros files: keep them a copy.
  `scripts/sync-shared.mjs` copies `website/shared/` to `public/shared/` and
  `book/src/images/` to `public/images/` at build. `src/styles/site.css` holds
  the layout and prose only.
- Flat: 1px rules, no shadow, no gradient, no radius. Mono labels in capitals,
  numbered panels (`src/components/Panel.astro`), a strip of key figures,
  status chips, box-drawing diagrams.
- One colour, one meaning: green is the accent and "passed", amber an injected
  fault, red a failed assertion or a dead process, cyan simulated time. A mark
  never uses colour alone: it also has a glyph or a label. Each text colour
  has 4.5:1 or more on the page and on a panel (`npm test` checks it).
- A few terminal details: a `$` prompt before the kicker, a block cursor after
  the home title, a terminal frame with the seed around each live run. No
  glow, no scanlines.
- Every page works on a phone. Check each page you change at 390 px and
  1280 px, light and dark, with no horizontal scroll of the page, and put the
  screenshots in the pull request.

## Writing

ASD-STE100 Simplified Technical English for the new pages. Name a symbol only
if it exists in the code. A code sample comes from a file that CI compiles. No
bench figures and no performance numbers.
