// After `astro build` (#415 (new site with the book)):
//
// 1. Write the redirects of the old mdbook addresses that have no page now:
//    `welcome.html` goes to the home page, `print.html` and `toc.html` to the
//    Guide. An mdbook chapter keeps its own address, so it needs none.
// 2. Check each link inside the site: a link to a page or a file of the site
//    that is not in dist/ fails the build.
// 3. Check that each old book address still answers (#420 (finish)): the
//    `.html` of each chapter of SUMMARY.md, and each redirect.
// 4. Check each link to the site in the READMEs and the Cargo.toml files of
//    the repository: the crates and docs.rs link to these pages.
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { chapters, pathOf, ROOT } from "../src/lib/book.mjs";

const SITE = "https://pierrez.github.io/moonpool/";

const BASE = "/moonpool/";
const dist = fileURLToPath(new URL("../dist/", import.meta.url));

export const REDIRECTS = { "welcome.html": "", "print.html": "guide/", "toc.html": "guide/" };

export function redirectPage(to) {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Moved</title>` +
    `<meta http-equiv="refresh" content="0; url=${to}"><link rel="canonical" href="${to}"></head>` +
    `<body><a href="${to}">${to}</a></body></html>\n`;
}

/**
 * The old book addresses (relative to the site) that are not in dist/: each
 * chapter of SUMMARY.md at its mdbook `.html` address, and each redirect.
 */
export function deadBookUrls(files, exists) {
  return [...new Set([...files.map(pathOf), ...Object.keys(REDIRECTS)])].filter((p) => !exists(p));
}

/** The pages of the site (relative, `index.html` added) that a text links to. */
export function siteLinks(text) {
  return [...text.matchAll(/https:\/\/pierrez\.github\.io\/moonpool\/([^)\s"'>]*)/g)].map((m) => {
    const path = m[1].split(/[?#]/)[0];
    return path === "" || path.endsWith("/") ? path + "index.html" : path;
  });
}

/** The internal links of one page that point at no file of dist/. */
export function brokenLinks(html, exists) {
  const out = [];
  for (const m of html.matchAll(/\s(?:href|src)="([^"]+)"/g)) {
    const link = m[1].replace(/&amp;/g, "&");
    if (!link.startsWith(BASE)) continue;
    let path = decodeURI(link.slice(BASE.length).split(/[?#]/)[0]);
    if (path === "" || path.endsWith("/")) path += "index.html";
    if (!exists(path)) out.push(link);
  }
  return out;
}

/**
 * The code blocks of one page that carry a colour theme of their own: a
 * `<Code>` without `theme="css-variables"` ignores the tokens of the site.
 */
export function foreignThemes(html) {
  return [...html.matchAll(/<pre class="astro-code ([\w-]+)"/g)].map((m) => m[1]).filter((t) => t !== "css-variables");
}

function walk(dir) {
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  for (const [from, to] of Object.entries(REDIRECTS)) {
    if (existsSync(join(dist, from))) throw new Error(`${from} is a page: it needs no redirect`);
    writeFileSync(join(dist, from), redirectPage(BASE + to));
  }
  const bad = [];
  for (const file of walk(dist).filter((f) => f.endsWith(".html"))) {
    // pagefind/ is written after this check; the search script loads it.
    const links = brokenLinks(readFileSync(file, "utf8"), (p) => p.startsWith("pagefind/") || existsSync(join(dist, p)));
    for (const l of links) bad.push(`${file.slice(dist.length)} -> ${l}`);
    for (const t of foreignThemes(readFileSync(file, "utf8"))) bad.push(`${file.slice(dist.length)}: a code block in theme ${t}, not css-variables`);
  }
  for (const p of deadBookUrls(chapters().map((c) => c.file), (p) => existsSync(join(dist, p)))) {
    bad.push(`the old book address ${BASE}${p} no longer answers`);
  }
  const docs = ["README.md", "Cargo.toml", ...readdirSync(join(ROOT, "crates")).flatMap((c) => [`crates/${c}/README.md`, `crates/${c}/Cargo.toml`])];
  for (const doc of docs.filter((d) => existsSync(join(ROOT, d)))) {
    for (const p of siteLinks(readFileSync(join(ROOT, doc), "utf8"))) {
      if (!existsSync(join(dist, p))) bad.push(`${doc} -> ${SITE}${p}`);
    }
  }
  if (bad.length) {
    console.error(`broken links or code themes inside the site:\n  ${bad.join("\n  ")}`);
    process.exit(1);
  }
  console.log(`redirects written; every link inside the site has its target; every old book address answers; every link to the site in the READMEs answers`);
}
