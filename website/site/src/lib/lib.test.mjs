// Unit tests of the build-time helpers: `npm test` (node --test).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { chapters, neighbours, parseSummary, pathOf, placeToc, rewriteLinks, splitCode, takeTitle, tocHtml } from "./book.mjs";
import { CRATE_NOTES, crateEntries } from "./crates.mjs";
import { builderCall, crateDocTest, moonpoolDocTest, moonpoolReadmeCrates, readmeCrates } from "./source.mjs";

const BASE = "/moonpool/";
const KNOWN = new Set(["index.md", "welcome.md", "part1/a.md", "part2/b.md"]);

test("parseSummary reads prefix chapters, parts and nesting", () => {
  const list = parseSummary(
    "# Summary\n\n[Welcome](./welcome.md)\n\n# Part I: Why\n\n- [A](./part1/a.md)\n  - [B](./part2/b.md)\n\n---\n",
  );
  assert.deepEqual(list, [
    { title: "Welcome", file: "welcome.md", depth: 0, part: "" },
    { title: "A", file: "part1/a.md", depth: 0, part: "Part I: Why" },
    { title: "B", file: "part2/b.md", depth: 1, part: "Part I: Why" },
  ]);
});

test("the real SUMMARY.md names each chapter once, and each file exists", () => {
  const list = chapters();
  assert.ok(list.length >= 80, `only ${list.length} chapters`);
  assert.equal(new Set(list.map((c) => c.file)).size, list.length);
  assert.ok(list.some((c) => c.file === "llms.md"));
});

test("a chapter keeps its mdbook address", () => {
  assert.equal(pathOf("part3-building/08-buggify.md"), "part3-building/08-buggify.html");
  assert.equal(pathOf("llms.md"), "llms.html");
});

test("rewriteLinks makes links absolute and keeps anchors", () => {
  const md = "See [b](../part2/b.md#x), [w](../welcome.md), [img](images/l.png) and [web](https://x.y/a.md).";
  assert.equal(
    rewriteLinks(md, "part1/a.md", BASE, KNOWN),
    "See [b](/moonpool/part2/b.html#x), [w](/moonpool/guide/), [img](/moonpool/part1/images/l.png) and [web](https://x.y/a.md).",
  );
});

test("rewriteLinks handles reference links and html attributes, not code", () => {
  const md = '[r]: ./b.md\n<a href="../part1/a.md">a</a>\n`[c](./x.md)`\n```\n[d](./y.md)\n```\n';
  assert.equal(
    rewriteLinks(md, "part2/c.md", BASE, KNOWN),
    '[r]: /moonpool/part2/b.html\n<a href="/moonpool/part1/a.html">a</a>\n`[c](./x.md)`\n```\n[d](./y.md)\n```\n',
  );
});

test("rewriteLinks refuses a link to a missing chapter", () => {
  assert.throws(() => rewriteLinks("[x](./nope.md)", "part1/a.md", BASE, KNOWN), /not a chapter/);
});

test("splitCode keeps the text whole", () => {
  const md = "a `b` c\n```rust\nlet x = 1;\n```\nd";
  assert.equal(splitCode(md).map(([t]) => t).join(""), md);
  assert.deepEqual(splitCode(md).map(([, c]) => c), [false, true, false, true, false]);
});

test("takeTitle takes the first heading only when it opens the file", () => {
  assert.deepEqual(takeTitle("# Hello\n\nbody"), { title: "Hello", body: "body" });
  assert.equal(takeTitle("text\n# Late\n").title, undefined);
});

test("the table of contents nests levels 2 to 4 and replaces the marker", () => {
  const hs = [
    { depth: 2, slug: "a", text: "A" },
    { depth: 3, slug: "b", text: "B <x>" },
    { depth: 2, slug: "c", text: "C" },
    { depth: 5, slug: "d", text: "D" },
  ];
  const toc = tocHtml(hs);
  assert.match(toc, /<ul><li><a href="#a">A<\/a><ul><li><a href="#b">B &lt;x&gt;<\/a><\/li><\/ul><\/li><li><a href="#c">C<\/a><\/li><\/ul>/);
  assert.ok(!toc.includes("#d"));
  assert.equal(placeToc("<p>x</p><!-- toc --><p>y</p>", []), "<p>x</p><p>y</p>");
});

test("the pager skips the two index files", () => {
  const list = parseSummary("[W](./welcome.md)\n[I](./index.md)\n[L](./llms.md)\n- [A](./part1/a.md)\n");
  assert.equal(neighbours(list, "llms.md").prev, undefined);
  assert.equal(neighbours(list, "llms.md").next.file, "part1/a.md");
});

test("crateDocTest takes the first rust block and drops hidden lines", () => {
  const src = "//! Intro\n//!\n//! ```rust\n//! let a = 1;\n//! # let hidden = 2;\n//! ```\n//! ```toml\n//! x\n//! ```\nfn main() {}\n";
  assert.equal(crateDocTest(src), "let a = 1;");
});

test("the moonpool doc test has the builder call the home page shows", () => {
  const call = builderCall(moonpoolDocTest());
  assert.match(call, /^let report = SimulationBuilder::new\(\)/);
  assert.match(call, /\.run\(\)\?;/);
  assert.match(call, /assert_eq!\(report\.failed_runs, 0\);$/);
});

test("the crate table of the README and the notes name the same crates", () => {
  const rows = moonpoolReadmeCrates();
  assert.ok(rows.length >= 10);
  const entries = crateEntries(rows);
  const names = new Set(chapters().map((c) => c.file));
  for (const e of entries) assert.ok(names.has(e.chapter), `${e.name}: chapter ${e.chapter} is not in SUMMARY.md`);
});

test("crateEntries fails on a crate with no note", () => {
  const rows = readmeCrates("| Crate | Purpose | Published |\n|--|--|--|\n| `new-crate` | x | yes |\n");
  assert.throws(() => crateEntries(rows, CRATE_NOTES), /no note for \[new-crate\]/);
});

test("tokens.css: each text colour has 4.5:1 or more on the page and on a panel", () => {
  const css = readFileSync(new URL("../../../shared/tokens.css", import.meta.url), "utf8");
  const themes = [
    css.slice(css.indexOf(":root {"), css.indexOf("@media (prefers-color-scheme: dark)")),
    css.slice(css.lastIndexOf(':root[data-theme="goyo-dark"]')),
  ];
  const lum = (hex) => {
    const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((m, n) => n - m); return (x + 0.05) / (y + 0.05); };
  for (const block of themes) {
    const tok = Object.fromEntries([...block.matchAll(/--([a-z-]+):\s*(#[0-9a-f]{6});/g)].map((m) => [m[1], m[2]]));
    for (const fg of ["fg", "muted", "accent", "fault", "fail", "info", "syn-keyword", "syn-string", "syn-comment", "syn-constant"]) {
      for (const bg of ["bg", "panel"]) {
        const r = ratio(tok[fg], tok[bg]);
        assert.ok(r >= 4.5, `--${fg} on --${bg}: ${r.toFixed(2)}:1`);
      }
    }
    assert.ok(ratio(tok["accent-fg"], tok.accent) >= 4.5, "--accent-fg on --accent");
  }
});

test("the home figures are counted in the code", async () => {
  const { homeFigures, enumVariants } = await import("./source.mjs");
  const f = homeFigures();
  assert.ok(f.providers.includes("TimeProvider") && f.providers.includes("StorageProvider"));
  assert.ok(f.chaos.includes("Network") && f.chaos.includes("Attrition"));
  assert.ok(f.assertions.includes("assert_sometimes"));
  assert.deepEqual(enumVariants("pub enum E {\n    /// doc\n    A(u8),\n    B {\n        x: u8,\n    },\n}\n", "E"), ["A", "B"]);
});

test("rustFn takes one top-level function, and the pages' functions exist", async () => {
  const { rustFn, repoFn } = await import("./source.mjs");
  const src = "use x;\n\nfn main() {\n    if a {\n    }\n}\n\nfn other() {}\n";
  assert.equal(rustFn(src, "main"), "fn main() {\n    if a {\n    }\n}");
  assert.throws(() => rustFn(src, "nope"));
  assert.match(repoFn("crates/moonpool-wasm-demo/src/bin/sim_register.rs", "main"), /check_determinism\(\)/);
});

test("the engine's fault tables and the register's hints read from the code", async () => {
  const { constNames, constStr, hintLabels, lineWith, repoFile } = await import("./source.mjs");
  const engine = repoFile("crates/moonpool-wasm-demo/src/engine.rs");
  assert.ok(constNames(engine, "NETWORK_FAULTS").includes("partition"));
  assert.ok(constNames(engine, "STORAGE_FAULTS").includes("crash_damage"));
  assert.match(constStr(engine, "HINT_STRUCK"), /^hint: /);
  assert.throws(() => constStr(engine, "NO_SUCH_CONST"));
  const register = repoFile("crates/moonpool-wasm-demo/src/register.rs");
  assert.deepEqual(hintLabels(register), ["register: record written, not synced", "register: write durable, ack not sent"]);
  assert.match(lineWith(register, "file.sync_all()"), /sync_all/);
  assert.throws(() => lineWith(register, "no such line"));
});

test("excerpt takes the lines between two markers, without their indent", async () => {
  const { excerpt } = await import("./source.mjs");
  const text = "fn a() {\n    let x = 1;\n    if x {\n        y();\n    }\n}\n";
  assert.equal(excerpt(text, "if x", "    }"), "if x {\n    y();\n}");
  assert.throws(() => excerpt(text, "nope", "}"));
});
