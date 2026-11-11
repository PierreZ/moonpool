// The moonpool site (#415 (new site with the book)): one Astro app in the
// paros style, with The Sim Book in it as the Guide. Every page uses
// src/layouts/Base.astro and the shared design system in website/shared/
// (copied to public/shared/ by scripts/sync-shared.mjs).
import { defineConfig } from "astro/config";

export default defineConfig({
  site: "https://pierrez.github.io",
  base: "/moonpool",
  // "preserve": a chapter keeps its mdbook address
  // (src/pages/[...slug].astro builds `part3-building/08-buggify.html`), and
  // a section page `crates/index.astro` builds `crates/index.html`.
  build: { format: "preserve" },
  trailingSlash: "ignore",
  markdown: {
    shikiConfig: { theme: "css-variables" },
  },
  // The Guide reads book/src/ at build time, one folder above the app.
  vite: { server: { fs: { allow: ["../.."] } } },
});
