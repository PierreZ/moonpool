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

/**
 * Rustdoc code blocks as a reader sees them: the hidden lines (`# …`) of a
 * Rust block are left out, and `compile_fail`, `no_run` and `ignore` blocks
 * are plain Rust.
 */
export function readerCode(md) {
  let rust = false;
  let fence = false;
  const out = [];
  for (const line of md.split("\n")) {
    const f = /^(\s*)```(.*)$/.exec(line);
    if (f) {
      if (!fence) {
        fence = true;
        const lang = f[2].trim();
        rust = lang === "" || /^rust\b|^compile_fail|^no_run|^ignore/.test(lang);
        out.push(rust ? `${f[1]}\`\`\`rust` : line);
      } else {
        fence = false;
        out.push(line);
      }
      continue;
    }
    if (fence && rust && /^\s*#(?: |$)/.test(line)) continue;
    out.push(line);
  }
  return out.join("\n");
}

/** Each heading of a markdown text `by` levels lower (`#` becomes `###` with 2). */
export function shiftHeadings(md, by) {
  let fence = false;
  return md
    .split("\n")
    .map((l) => {
      if (/^\s*```/.test(l)) fence = !fence;
      return !fence && /^#+ /.test(l) ? "#".repeat(by) + l : l;
    })
    .join("\n");
}

/** The index of the brace that closes the one at `open`. */
function closing(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === "{") depth++;
    if (text[i] === "}" && --depth === 0) return i;
  }
  throw new Error("a block does not close");
}

/** The body of the first `fn name(` of a file (any indent, any receiver). */
export function fnBody(text, name) {
  const start = text.search(new RegExp(`\\bfn ${name}\\(`));
  if (start < 0) throw new Error(`no fn ${name}`);
  const open = text.indexOf("{", text.indexOf(")", start));
  return text.slice(open + 1, closing(text, open));
}

/** The body of the match arm that starts with `pattern` (up to its `=> { … }`). */
function armBody(text, pattern) {
  const at = text.indexOf(pattern);
  if (at < 0) throw new Error(`no arm ${pattern}`);
  const arrow = text.indexOf("=> {", at);
  return text.slice(arrow + 4, closing(text, arrow + 3));
}

/** The argument of the call whose `(` is at `open`. */
function callArg(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === "(") depth++;
    if (text[i] === ")" && --depth === 0) return text.slice(open + 1, i);
  }
  throw new Error("a call does not close");
}

const WIDTH = { u8: 1, u16: 2, u32: 4, u64: 8, u128: 16 };

function writtenName(arg) {
  const a = arg.trim();
  if (/^KIND_/.test(a)) return "kind";
  const len = /(\w+)\.len\(\)/.exec(a);
  if (len) return `${len[1]}_len`;
  const call = /^\*?(\w+)\.(\w+)\(\)$/.exec(a);
  if (call) return call[2] === "get" ? call[1] : `${call[1]}.${call[2]}`;
  const plain = /^\*?(\w+)$/.exec(a);
  if (plain) return plain[1];
  throw new Error(`cannot name the field written from ${a}`);
}

/**
 * The fields that a `Writer` chain writes in one arm of an encoder, in order:
 * `{ name, size, when }`. `size` is null for bytes of any length (a `bytes`
 * call, or a nested `match` that writes one of several shapes, named after
 * what it matches). A field written inside an `if` carries its condition in
 * `when`. `branch` picks one arm of a nested `match` (such as
 * `WireOutcome::Ok`), and `literals` names the fields written from a number.
 */
export function writerLayout(text, fn, arm, { branch, literals = [] } = {}) {
  const body = armBody(fnBody(text, fn), arm);
  const rows = [];
  let lit = 0;
  const skipTo = (re, from) => {
    re.lastIndex = from;
    return re.exec(body);
  };
  const scan = (from, to, when) => {
    const re = /(\bmatch (\w+) \{|\bif ([^{]+)\{|\.(u8|u16|u32|u64|u128|bytes|socket_addr)\()/g;
    let pos = from;
    for (;;) {
      const m = skipTo(re, pos);
      if (!m || m.index >= to) return;
      if (m[2]) {
        const open = m.index + m[0].length - 1;
        const end = closing(body, open);
        const at = branch ? body.indexOf(branch, open) : -1;
        if (at >= 0 && at < end) {
          const arrow = body.indexOf("=> {", at);
          scan(arrow + 4, closing(body, arrow + 3), when);
        } else rows.push({ name: m[2], size: null, when });
        pos = end + 1;
      } else if (m[3]) {
        const open = m.index + m[0].length - 1;
        const end = closing(body, open);
        scan(open + 1, end, m[3].trim());
        pos = end + 1;
      } else {
        const open = m.index + m[0].length - 1;
        const arg = callArg(body, open);
        const name = /^\s*\d+\s*$/.test(arg) ? (literals[lit++] ?? arg.trim()) : writtenName(arg);
        rows.push({ name, size: WIDTH[m[4]] ?? null, when });
        pos = open + arg.length + 2;
      }
    }
  };
  scan(0, body.length, undefined);
  if (!rows.length) throw new Error(`${arm} writes no field`);
  return rows;
}

/** The golden vectors of a fixture file: `name hex` lines, `#` comments left out. */
export function fixtures(text) {
  const out = {};
  for (const line of text.split("\n")) {
    const m = /^([\w-]+) ([0-9a-f]+)$/.exec(line.trim());
    if (m) out[m[1]] = m[2];
  }
  return out;
}

/** A little-endian unsigned integer of hex bytes, as a decimal string. */
export function leValue(hex) {
  let v = 0n;
  for (let i = hex.length - 2; i >= 0; i -= 2) v = (v << 8n) | BigInt(parseInt(hex.slice(i, i + 2), 16));
  return v.toString();
}

/**
 * A layout (`writerLayout`) laid over one golden vector: each field with its
 * offset, its size and its bytes. A field of any length takes the value of
 * the field `<name>_len` before it, or else all the bytes that are left. A
 * field with a condition is kept only when `when` is true. The vector must
 * end where the last field ends.
 */
export function fillBytes(rows, hex, { when = false } = {}) {
  const out = [];
  let at = 0;
  const total = hex.length / 2;
  rows.filter((r) => !r.when || when).forEach((r, k, all) => {
    let size = r.size;
    if (size === null) {
      const len = out.find((o) => o.name === `${r.name}_len`);
      if (len) size = Number(len.value);
      else if (all.slice(k + 1).some((x) => x.size === null)) throw new Error(`${r.name}: two fields of any length`);
      else size = total - at - all.slice(k + 1).reduce((s, x) => s + x.size, 0);
    }
    const bytes = hex.slice(at * 2, (at + size) * 2);
    if (bytes.length !== size * 2) throw new Error(`${r.name}: the vector ends inside the field`);
    out.push({ name: r.name, at, size, bytes, value: r.size !== null ? leValue(bytes) : undefined, when: r.when });
    at += size;
  });
  if (at !== total) throw new Error(`the vector has ${total - at} bytes after the last field`);
  return out;
}

/**
 * The variants of `enum Name` with their doc comments, in order:
 * `{ name, doc, fields, value }`, where `fields` holds the doc of each named
 * field and `value` is the discriminant of `Name = 3` (undefined without one).
 */
export function variantDocs(text, name) {
  const at = text.search(new RegExp(`enum ${name} \\{`));
  if (at < 0) throw new Error(`no enum ${name}`);
  const open = text.indexOf("{", at);
  const body = text.slice(open + 1, closing(text, open));
  const out = [];
  let doc = [];
  let depth = 0;
  let current;
  for (const line of body.split("\n")) {
    const d = /^\s*\/\/\/ ?(.*)$/.exec(line);
    if (d) {
      doc.push(d[1]);
      continue;
    }
    if (depth === 0) {
      const v = /^\s*([A-Z]\w*)\b(?:\s*=\s*(\d+))?/.exec(line);
      if (v) {
        current = { name: v[1], doc: doc.join("\n").trim(), fields: {}, value: v[2] === undefined ? undefined : Number(v[2]) };
        out.push(current);
      }
    } else {
      const f = /^\s*(\w+):/.exec(line);
      if (f && current && doc.length) current.fields[f[1]] = doc.join(" ").trim();
    }
    depth += (line.match(/\{/g) ?? []).length - (line.match(/\}/g) ?? []).length;
    if (!/^\s*#\[/.test(line)) doc = [];
  }
  return out;
}
