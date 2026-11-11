---
name: update-the-site
description: Change the moonpool website (website/site/, an Astro app with The Sim Book in it as the Guide) - a page, the home run, the crates page, the Guide's sidebar or search, the design tokens, CI and deploy - and check it at 390 and 1280 px in light and dark. Use when touching website/, when a chapter link or SUMMARY.md change breaks the site build, or when a change moves a fact that a page reads from the code (the moonpool doc test, the README crate table, the Chaos variants, the wasm demo's runs).
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
nix develop --command bash -c 'cd website/site && npm ci && npm test && npm run build'
```

`npm run build` fails on a link to a missing chapter, on a broken link inside
the site, and on a crate of the README table with no entry in
`src/lib/crates.mjs`. `site_runs` fails once when the recorded runs of the home
page were stale, and writes them again: commit `src/runs/home.json`.

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
