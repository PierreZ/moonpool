---
name: update-the-site
description: Change the moonpool website (website/site/, an Astro app with The Sim Book in it as the Guide) - a page, the home run, a technique page, the playground, an extra page, the crates page, the mark and the icons, the Guide's sidebar or search, the design tokens, CI and deploy - and check it at 390 and 1280 px in light and dark. Use when touching website/, when a chapter link or SUMMARY.md change breaks the site build, or when a change moves a fact that a page reads from the code (the moonpool doc test, the README crate table, the Chaos variants, the engine's recorded runs).
---

# Update the site

Read `website/site/AGENTS.md` first: it has the map of the pages, the rules of
the Guide, the facts that pages read from the code, and the style rules. The
shared rules of the makeover (site map, tokens, animation and URL rules) are
in issue #414 (website makeover).

## Build and test

```bash
nix develop --command website/site/scripts/build-wasm-demo.sh   # the wasm demo, once
nix develop --command cargo test -p moonpool-wasm-demo --test site_runs
nix develop --command scripts/check-site-wasm.sh 1 7 42 256   # native = wasm32
nix develop --command bash -c 'cd website/site && npm ci && npm test && npm run build'
```

`npm run build` fails on a link to a missing chapter, on a broken link inside
the site, on an old book address that no longer answers, on a link to the
site in a README or a `Cargo.toml` that has no page, on a crate of the README
table with no entry in `src/lib/crates.mjs`, and on a marker or a doc section
that a page reads from the code and that is gone. `site_runs` fails once when the recorded runs of a page
were stale, and writes them again: commit `src/runs/*.json`.

## A live run on a new page

1. Add the page's configs to `pages()` in
   `crates/moonpool-wasm-demo/tests/site_runs.rs` and run the test twice.
2. Inline the JSON in the page (`<script type="application/json">`, with `<`
   escaped) and mount the player: `mount(root, run, opts)` from
   `public/player.js`, or `new Player(...)` for a custom layout as
   `public/compare.js` does.
3. If the player needs a fact that the run does not hold, add a `tracing`
   event to the scenario (`src/register.rs`) or a field to `Run`
   (`src/engine.rs`, bump `MODEL`), then read it in `public/run-model.js`
   with a test in `src/lib/player.test.mjs`. Never compute it in JavaScript.

## A technique page

1. Add the technique to `TECHNIQUES` in `src/lib/techniques.mjs` with its
   `path`; the home page, the techniques index and the nav read that list.
2. Write `src/pages/techniques/<name>/index.astro` on
   `src/layouts/Technique.astro`: the problem, the technique, the live run
   (above), the code, the Guide chapters. Read each code sample and each
   name from the files (`excerpt`, `lineWith`, `constNames` of
   `src/lib/source.mjs`), never copy it.
3. Put the page's script in `public/<name>.js` and add its row to the table
   of pages in `website/site/AGENTS.md`.
4. Link each Guide chapter of the page back to it.

## An extra page

An extra page describes one crate in depth from its own code
(`src/pages/extras/<name>/index.astro` on `src/layouts/Extra.astro`, listed
in `src/lib/extras.mjs`). Render its module docs with `moduleDoc`,
`docSection`, `unlink` and `readerCode` of `src/lib/rustdoc.mjs` and
`renderMarkdown` of `src/lib/book.mjs`. Draw an on-disk structure from its
`encode` function (`encodeLayout`, `src/components/ByteLayout.astro`) and a
wire frame from its `Writer` calls laid over a golden vector of the crate's
tests (`writerLayout`, `fillBytes`, `src/components/FrameLayout.astro`).

## The mark, the icons and the social card

The mark is `src/components/Mark.astro` (one colour, `currentColor`). After a
change to the mark or to the accent, run `node website/site/scripts/icons.mjs`
(it needs Playwright) and commit what it writes in `public/`: the favicons
and `social-card.png`. `npm test` fails while `public/favicon.svg` does not
match the mark.

## Look at it

Serve `dist/` under `/moonpool/` and take screenshots with Playwright at 390
and 1280 px, `colorScheme` light and dark. Check that
`document.documentElement.scrollWidth` is not more than `clientWidth`. Put the
screenshots in the pull request.

```bash
mkdir -p /tmp/serve && ln -sfn "$PWD/website/site/dist" /tmp/serve/moonpool
(cd /tmp/serve && python3 -m http.server 8765) &
```

## Rules that are easy to break

- `website/shared/tokens.css` is the only file with a colour. Do not edit
  `base.css` or `theme.js`: they are copies of the paros files.
- The URL of a chapter never changes. A new section page is
  `src/pages/<section>/index.astro`, so it is served at `/<section>/`.
- No simulation logic in JavaScript: a page draws a recorded run, then the
  same run from wasm. A recorded run is written by a Rust test.
- A pull request that touches only `website/`, `book/` or Markdown runs only
  the `website` job of CI (the `scope` job in `.github/workflows/rust.yml`).
