// Facts that the extra pages read from a crate's Rust source at build time
// (#428 (journal page), #427 (rpc page)): the module docs (`//!`), one
// section of them, and the byte layout an `encode` function writes. A page
// shows the code's own words and numbers, so it cannot go stale: a section
// or a layout that is gone fails the build.

/** The module doc (`//!` lines) of a Rust file, as markdown. */
export function moduleDoc(text) {
  return text
    .split("\n")
    .filter((l) => l.startsWith("//!"))
    .map((l) => l.replace(/^\/\/! ?/, ""))
    .join("\n");
}

/**
 * The part of a doc under `# heading` (any level), up to the next heading of
 * the same or a higher level. With no heading, the part before the first one.
 */
export function docSection(doc, heading) {
  const lines = doc.split("\n");
  let inFence = false;
  const heads = [];
  lines.forEach((l, i) => {
    if (/^```/.test(l)) inFence = !inFence;
    const m = !inFence && /^(#+) (.*)$/.exec(l);
    if (m) heads.push({ i, level: m[1].length, text: m[2].trim() });
  });
  if (heading === undefined) return lines.slice(0, heads.length ? heads[0].i : lines.length).join("\n").trim();
  const h = heads.find((x) => x.text === heading);
  if (!h) throw new Error(`no section "${heading}" in the doc`);
  const next = heads.find((x) => x.i > h.i && x.level <= h.level);
  return lines.slice(h.i + 1, next ? next.i : lines.length).join("\n").trim();
}

/**
 * Rustdoc markdown as plain markdown: an intra-doc link keeps its text and
 * loses its target (`[`State::Corrupt`]` and `[`x`](crate::y)` become
 * `` `State::Corrupt` `` and `` `x` ``); a web link stays.
 */
export function unlink(md) {
  return md
    .replace(/\[([^\]]+)\]\((?!https?:)[^)]*\)/g, "$1")
    .replace(/\[(`[^`\]]+`)\](?!\()/g, "$1")
    .replace(/^```rust,ignore$/gm, "```rust");
}

/** The value of a `const NAME: usize = expr;` (or u64), evaluated. */
export function constValue(text, name, seen = new Set()) {
  if (/^\d+$/.test(name)) return Number(name);
  if (seen.has(name)) throw new Error(`const ${name} refers to itself`);
  const m = new RegExp(`const ${name}: [\\w]+ = ([^;]+);`).exec(text);
  if (!m) throw new Error(`no const ${name}`);
  return evalExpr(text, m[1], new Set([...seen, name]));
}

function evalExpr(text, expr, seen) {
  const plain = expr
    .replace(/(\d)_(?=\d)/g, "$1")
    .replace(/ as \w+/g, "")
    .replace(/\b[A-Z][A-Z0-9_]*\b/g, (n) => String(constValue(text, n, seen)));
  if (!/^[\d\s+\-*/()]+$/.test(plain)) throw new Error(`cannot evaluate ${expr}`);
  return Function(`"use strict"; return (${plain});`)();
}

/** The body of `fn name` inside `impl Type`, whitespace folded. */
function implFn(text, type, fn) {
  const at = text.search(new RegExp(`\\nimpl(?:<[^>]*>)? ${type} \\{`));
  if (at < 0) throw new Error(`no impl ${type}`);
  const start = text.indexOf(`fn ${fn}(`, at);
  if (start < 0) throw new Error(`no fn ${fn} in impl ${type}`);
  let depth = 0;
  let i = text.indexOf("{", start);
  const open = i;
  for (; i < text.length; i++) {
    if (text[i] === "{") depth++;
    if (text[i] === "}" && --depth === 0) break;
  }
  return text.slice(open + 1, i).replace(/\s+/g, " ");
}

function fieldName(value) {
  if (/\.len\(\)/.test(value) && !/^self\.\w+$/.test(value)) return "len";
  const self = /self\.(\w+)(?:\.(\w+))?/.exec(value);
  if (self) return self[2] && !/^\d+$/.test(self[2]) ? self[2] : self[1];
  if (/^crc\(|^sum$/.test(value.trim())) return "crc";
  const magic = /^([A-Z]+)_MAGIC$/.exec(value.trim());
  if (magic) return "magic";
  if (/^VERSION$/.test(value.trim())) return "version";
  return value.trim();
}

/**
 * The byte layout that `impl Type { fn encode }` writes: one row per field,
 * `{ at, size, name }`, sorted by offset. It reads `put_uN(buf, at, value)`,
 * `buf[at] = value` and `buf[a..b].copy_from_slice(&value)`.
 */
export function encodeLayout(text, type, fn = "encode") {
  const body = implFn(text, type, fn);
  const rows = [];
  for (const m of body.matchAll(/put_u(\d+)\(\s*\w+,\s*(\w+),\s*(.+?),?\s*\);/g)) {
    rows.push({ at: constValue(text, m[2]), size: Number(m[1]) / 8, name: fieldName(m[3]) });
  }
  for (const m of body.matchAll(/\b\w+\[(\w+)\] = ([^;]+);/g)) {
    rows.push({ at: constValue(text, m[1]), size: 1, name: fieldName(m[2]) });
  }
  for (const m of body.matchAll(/\b\w+\[(\w+)\.\.([^\]]+)\]\.copy_from_slice\(&([^)]+)\);/g)) {
    const at = constValue(text, m[1]);
    const end = /len\(\)/.test(m[2]) ? null : evalExpr(text, m[2], new Set());
    rows.push({ at, size: end === null ? null : end - at, name: fieldName(m[3]) });
  }
  rows.sort((a, b) => a.at - b.at);
  if (!rows.length) throw new Error(`impl ${type} fn ${fn} writes no field`);
  return rows;
}

/** The fields of `Self { a: 1, b: 2 }` returned by `impl Type { fn name }`. */
export function implFields(text, type, fn) {
  const body = implFn(text, type, fn);
  const out = {};
  for (const m of body.matchAll(/(\w+): ([\d_]+),/g)) out[m[1]] = Number(m[2].replace(/_/g, ""));
  return out;
}

/**
 * The doc comment (`///` lines) above an item, such as `struct Record`, as
 * markdown, with its code blocks left out (a page draws them itself).
 */
export function itemDoc(text, item) {
  const lines = text.split("\n");
  const at = lines.findIndex((l) => new RegExp(`^\\s*(pub(\\([a-z]+\\))? )?${item.replace(/ /g, "\\s+")}\\b`).test(l));
  if (at < 0) throw new Error(`no ${item}`);
  let i = at - 1;
  while (i >= 0 && /^\s*#\[/.test(lines[i])) i--;
  const doc = [];
  for (; i >= 0 && /^\s*\/\/\//.test(lines[i]); i--) doc.unshift(lines[i].replace(/^\s*\/\/\/ ?/, ""));
  return doc.join("\n").replace(/```[\s\S]*?```\n?/g, "").trim();
}

/** The doc comment of each field of `struct Name`, by field name. */
export function fieldDocs(text, struct) {
  const at = text.search(new RegExp(`struct ${struct} \\{`));
  if (at < 0) throw new Error(`no struct ${struct}`);
  const body = text.slice(at, text.indexOf("\n}", at));
  const out = {};
  let doc = [];
  for (const line of body.split("\n").slice(1)) {
    const d = /^\s*\/\/\/ ?(.*)$/.exec(line);
    if (d) {
      doc.push(d[1]);
      continue;
    }
    const f = /^\s*pub(?:\([a-z]+\))? (\w+):/.exec(line);
    if (f && doc.length) out[f[1]] = doc.join(" ");
    doc = [];
  }
  return out;
}
