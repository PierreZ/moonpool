import assert from "node:assert/strict";
import { test } from "node:test";
import { brokenLinks, redirectPage, REDIRECTS } from "./redirects.mjs";

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
