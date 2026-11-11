// The Sim Book inside the site (#415 (new site with the book)).
//
// The chapters stay in book/src/ and keep their mdbook addresses: the file
// `part3-building/08-buggify.md` is the page `part3-building/08-buggify.html`.
// book/src/SUMMARY.md is the one source of the order of the chapters (the
// sidebar and the pager read it). This module is plain JavaScript so that
// `npm test` (node --test) checks it without a build.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, normalize } from "node:path/posix";
import { resolve } from "node:path";

/** The root of the repository. npm runs the build and the tests in website/site/
 *  (the bundler moves this module, so its own address does not help). */
export const ROOT = resolve(process.cwd(), "../..") + "/";

/** The folder of the book. */
export const BOOK = ROOT + "book/src/";

/**
 * Read SUMMARY.md: the prefix chapters, the parts (`# Part I: …`) and the
 * nested chapters (`- [Title](./file.md)`, two spaces per level).
 * @returns {{ title: string, file: string, depth: number, part: string }[]}
 */
export function parseSummary(text) {
  const out = [];
  let part = "";
  for (const line of text.split("\n")) {
    const head = /^#\s+(.+)$/.exec(line);
    if (head) {
      if (head[1].trim() !== "Summary") part = head[1].trim();
      continue;
    }
    const item = /^(\s*)(?:-\s+)?\[([^\]]+)\]\(([^)]+)\)\s*$/.exec(line);
    if (!item) continue;
    const file = normalize(item[3].replace(/^\.\//, ""));
    out.push({ title: item[2], file, depth: item[1].length / 2, part });
  }
  return out;
}

/** `part3-building/08-buggify.md` -> `part3-building/08-buggify` (the route). */
export function slugOf(file) {
  return file.replace(/\.md$/, "");
}

/** `part3-building/08-buggify.md` -> `part3-building/08-buggify.html` (the address). */
export function pathOf(file) {
  return slugOf(file) + ".html";
}

/** The two book files that are not chapter pages: `/guide/` shows them. */
export const GUIDE_INDEX = ["welcome.md", "index.md"];

const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|#|\/)/i;

/**
 * Make each relative link of a chapter absolute: `../part2/x.md#a` in
 * `part3/y.md` becomes `<base>part2/x.html#a`, so the same text works on a
 * chapter page and on `/guide/`. A link to a `.md` file that is not in the
 * book is an error. Code (fenced blocks and inline code) is not changed.
 * @param {string} md the chapter text
 * @param {string} file the chapter, relative to book/src/
 * @param {string} base the base of the site, with a slash at the end
 * @param {Set<string>} known the chapter files
 */
export function rewriteLinks(md, file, base, known) {
  const dir = dirname(file);
  const fix = (target) => {
    if (EXTERNAL.test(target)) return target;
    const [path, hash = ""] = target.split(/(?=#)/);
    const resolved = normalize(join(dir, path));
    if (resolved.startsWith("..")) throw new Error(`${file}: link ${target} leaves the book`);
    if (path.endsWith(".md")) {
      if (!known.has(resolved)) throw new Error(`${file}: link to ${target}, which is not a chapter`);
      if (GUIDE_INDEX.includes(resolved)) return `${base}guide/${hash}`;
      return base + pathOf(resolved) + hash;
    }
    return base + resolved + hash;
  };
  return splitCode(md)
    .map(([text, code]) => {
      if (code) return text;
      return text
        .replace(/(\]\()([^)\s]+)((?:\s+"[^"]*")?\))/g, (_, a, t, b) => a + fix(t) + b)
        .replace(/^(\[[^\]]+\]:\s*)(\S+)/gm, (_, a, t) => a + fix(t))
        .replace(/(\s(?:href|src)=")([^"]+)(")/g, (_, a, t, b) => a + fix(t) + b);
    })
    .join("");
}

/** Split Markdown into [text, isCode] pieces: fenced blocks and inline code are code. */
export function splitCode(md) {
  const parts = [];
  const re = /(^(```|~~~)[^\n]*\n[\s\S]*?^\2[^\n]*$|`[^`\n]+`)/gm;
  let last = 0;
  for (const m of md.matchAll(re)) {
    if (m.index > last) parts.push([md.slice(last, m.index), false]);
    parts.push([m[0], true]);
    last = m.index + m[0].length;
  }
  if (last < md.length) parts.push([md.slice(last), false]);
  return parts;
}

/** Take the first `# Title` line off a chapter: the page layout shows the title. */
export function takeTitle(md) {
  const m = /^#\s+(.+?)\s*\n/m.exec(md);
  if (!m || md.slice(0, m.index).trim() !== "") return { title: undefined, body: md };
  return { title: m[1], body: md.slice(m.index + m[0].length) };
}

function escape(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/**
 * The table of contents of a page, from its headings (levels 2 to 4), as
 * mdbook-toc made it. It goes where the chapter has its `<!-- toc -->` marker.
 * @param {{ depth: number, slug: string, text: string }[]} headings
 */
export function tocHtml(headings) {
  const hs = headings.filter((h) => h.depth >= 2 && h.depth <= 4);
  if (hs.length === 0) return "";
  // Each level is either closed or holds an open <li>.
  let html = "";
  let level = 1;
  let open = false;
  for (const h of hs) {
    if (h.depth > level) {
      while (level < h.depth) { html += "<ul>"; level++; }
    } else {
      if (open) html += "</li>";
      while (level > h.depth) { html += "</ul></li>"; level--; }
    }
    html += `<li><a href="#${h.slug}">${escape(h.text)}</a>`;
    open = true;
  }
  if (open) html += "</li>";
  while (level > 2) { html += "</ul></li>"; level--; }
  html += "</ul>";
  return `<nav class="toc" aria-label="On this page"><div class="label">On this page</div>${html}</nav>`;
}

/** Put the table of contents in place of the marker (the first one only). */
export function placeToc(html, headings) {
  return html.replace(/<!--\s*toc\s*-->/, tocHtml(headings)).replace(/<!--\s*toc\s*-->/g, "");
}

let processor;
async function markdown() {
  if (!processor) {
    const { createMarkdownProcessor } = await import("@astrojs/markdown-remark");
    processor = await createMarkdownProcessor({ shikiConfig: { theme: "css-variables" } });
  }
  return processor;
}

/** The chapters in SUMMARY order. */
export function chapters() {
  return parseSummary(readFileSync(join(BOOK, "SUMMARY.md"), "utf8"));
}

/**
 * Render one book file to HTML: links made absolute, the title taken off,
 * the table of contents in place of its marker.
 * @param {string} file relative to book/src/
 * @param {string} base the base of the site, with a slash at the end
 */
export async function renderChapter(file, base) {
  const known = new Set(chapters().map((c) => c.file));
  const path = join(BOOK, file);
  if (!existsSync(path)) throw new Error(`SUMMARY.md names ${file}, which does not exist`);
  const { title, body } = takeTitle(readFileSync(path, "utf8"));
  const md = rewriteLinks(body, file, base, known);
  const { code, metadata } = await (await markdown()).render(md);
  return { title, html: placeToc(code, metadata.headings), headings: metadata.headings };
}

/** The neighbours of a chapter in SUMMARY order, for the pager. */
export function neighbours(list, file) {
  const pages = list.filter((c) => !GUIDE_INDEX.includes(c.file));
  const i = pages.findIndex((c) => c.file === file);
  return { prev: i > 0 ? pages[i - 1] : undefined, next: i >= 0 ? pages[i + 1] : undefined };
}
