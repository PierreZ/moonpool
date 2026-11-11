// Facts that the site reads from the code at build time, so a page never
// holds a copy that can go stale (#415 (new site with the book)).
import { readFileSync } from "node:fs";
import { ROOT } from "./book.mjs";

/**
 * The first ```rust block of the crate-level doc comment (`//!`) of a file:
 * the doc test that `cargo test --doc` compiles. Hidden lines (`# …`) are
 * dropped, as rustdoc drops them.
 */
export function crateDocTest(text) {
  const doc = text
    .split("\n")
    .filter((l) => l.startsWith("//!"))
    .map((l) => l.replace(/^\/\/! ?/, ""));
  const start = doc.findIndex((l) => /^```(rust)?\s*$/.test(l));
  if (start < 0) throw new Error("no ```rust block in the crate doc comment");
  const end = doc.findIndex((l, i) => i > start && /^```\s*$/.test(l));
  if (end < 0) throw new Error("the ```rust block of the crate doc comment does not end");
  return doc
    .slice(start + 1, end)
    .filter((l) => !/^#(\s|$)/.test(l))
    .join("\n");
}

/**
 * The part of the doc test that the home page shows: the builder call, from
 * `let report = SimulationBuilder::new()` to the assertion on the report.
 */
export function builderCall(test) {
  const lines = test.split("\n");
  const start = lines.findIndex((l) => l.includes("SimulationBuilder::new()"));
  const end = lines.findIndex((l, i) => i > start && l.includes("report.failed_runs"));
  if (start < 0 || end < 0) throw new Error("the doc test has no SimulationBuilder call and report check");
  return lines.slice(start, end + 1).join("\n");
}

/** The crate-level doc test of `moonpool` (crates/moonpool/src/lib.rs). */
export function moonpoolDocTest() {
  return crateDocTest(readFileSync(ROOT + "crates/moonpool/src/lib.rs", "utf8"));
}

/**
 * The crate table of the root README.md (`| Crate | Purpose | Published |`).
 * @returns {{ name: string, purpose: string, published: boolean }[]}
 */
export function readmeCrates(text) {
  const lines = text.split("\n");
  const head = lines.findIndex((l) => /^\|\s*Crate\s*\|\s*Purpose\s*\|\s*Published\s*\|/.test(l));
  if (head < 0) throw new Error("README.md has no crate table");
  const rows = [];
  for (const line of lines.slice(head + 2)) {
    if (!line.startsWith("|")) break;
    const cells = line.split("|").slice(1, -1).map((c) => c.trim());
    rows.push({ name: cells[0].replace(/`/g, ""), purpose: cells[1], published: cells[2] === "yes" });
  }
  return rows;
}

export function moonpoolReadmeCrates() {
  return readmeCrates(readFileSync(ROOT + "README.md", "utf8"));
}

/** The variants of `pub enum Chaos { … }`: the chaos surfaces. */
export function enumVariants(text, name) {
  const start = text.indexOf(`pub enum ${name} {`);
  if (start < 0) throw new Error(`no pub enum ${name}`);
  const body = text.slice(start, text.indexOf("\n}", start));
  return [...body.matchAll(/^ {4}([A-Z][A-Za-z]*)\b/gm)].map((m) => m[1]);
}

/** The associated provider types of `pub trait Providers`. */
export function providerTypes(text) {
  const start = text.indexOf("pub trait Providers");
  if (start < 0) throw new Error("no pub trait Providers");
  const body = text.slice(start, text.indexOf("\n}", start));
  return [...body.matchAll(/^\s+type (\w+): (\w+Provider);/gm)].map((m) => m[2]);
}

/** The key figures of the home page, counted in the code. */
export function homeFigures() {
  const read = (p) => readFileSync(ROOT + p, "utf8");
  return {
    providers: providerTypes(read("crates/moonpool-core/src/providers.rs")),
    chaos: enumVariants(read("crates/moonpool-sim/src/runner/config.rs"), "Chaos"),
    assertions: [...read("crates/moonpool-sim/src/chaos/assertions.rs").matchAll(/macro_rules! (assert_\w+)/g)].map((m) => m[1]),
  };
}

/** The `major.minor` of the workspace version (Cargo.toml), as a dependency line uses it. */
export function workspaceVersion() {
  const m = /^version = "(\d+\.\d+)\.\d+"/m.exec(readFileSync(ROOT + "Cargo.toml", "utf8"));
  if (!m) throw new Error("Cargo.toml has no workspace version");
  return m[1];
}

/**
 * The text of a top-level `fn name` of a Rust file, from its line to the `}`
 * at the start of a line that closes it. A page shows code that CI compiles.
 */
export function rustFn(text, name) {
  const start = text.search(new RegExp("^(pub )?fn " + name + "\\b", "m"));
  if (start < 0) throw new Error(`no top-level fn ${name}`);
  const end = text.indexOf("\n}", start);
  if (end < 0) throw new Error(`fn ${name} does not close`);
  return text.slice(start, end + 2);
}

/** `rustFn` over a file of the repository. */
export function repoFn(path, name) {
  return rustFn(readFileSync(ROOT + path, "utf8"), name);
}
