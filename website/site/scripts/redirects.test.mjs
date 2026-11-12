import assert from "node:assert/strict";
import { test } from "node:test";
import { brokenLinks, deadBookUrls, siteLinks, redirectPage, REDIRECTS } from "./redirects.mjs";

test("the old mdbook pages with no page now redirect", () => {
  assert.equal(REDIRECTS["welcome.html"], "");
  assert.match(redirectPage("/moonpool/"), /url=\/moonpool\//);
});

test("brokenLinks checks only the links inside the site", () => {
  const html = '<a href="/moonpool/a.html#x">a</a> <a href="/moonpool/guide/">g</a> <img src="/moonpool/nope.png"> <a href="https://x.y/">x</a> <a href="#top">t</a>';
  const have = new Set(["a.html", "guide/index.html"]);
  assert.deepEqual(brokenLinks(html, (p) => have.has(p)), ["/moonpool/nope.png"]);
});

test("a code block in a theme other than css-variables is reported", async () => {
  const { foreignThemes } = await import("./redirects.mjs");
  assert.deepEqual(foreignThemes('<pre class="astro-code css-variables" style="">'), []);
  assert.deepEqual(foreignThemes('<pre class="astro-code github-dark" style="">'), ["github-dark"]);
});

test("each old book address must answer: chapters and redirects", () => {
  const have = new Set(["a/b.html", "welcome.html", "print.html", "toc.html"]);
  assert.deepEqual(deadBookUrls(["a/b.md", "c.md"], (p) => have.has(p)), ["c.html"]);
  assert.deepEqual(deadBookUrls(["a/b.md"], () => false), ["a/b.html", "welcome.html", "print.html", "toc.html"]);
});

test("the links to the site in a README, as pages of dist/", () => {
  const md = "[a](https://pierrez.github.io/moonpool/) [b](https://pierrez.github.io/moonpool/part3/x.html#h) <https://pierrez.github.io/moonpool/techniques/swarm/?seed=4> homepage = \"https://pierrez.github.io/moonpool/\" [c](https://pierrez.github.io/paros/)";
  assert.deepEqual(siteLinks(md), ["index.html", "part3/x.html", "techniques/swarm/index.html", "index.html"]);
});
