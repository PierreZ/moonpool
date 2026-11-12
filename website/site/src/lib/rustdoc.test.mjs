import { test } from "node:test";
import assert from "node:assert/strict";
import { constValue, docSection, encodeLayout, implFields, moduleDoc, unlink } from "./rustdoc.mjs";
import { repoFile } from "./source.mjs";

test("a doc section runs to the next heading of its level, and skips code fences", () => {
  const doc = "intro\n# A\none\n```\n# not a heading\n```\n## A.1\ntwo\n# B\nthree";
  assert.equal(docSection(doc), "intro");
  assert.equal(docSection(doc, "A"), "one\n```\n# not a heading\n```\n## A.1\ntwo");
  assert.equal(docSection(doc, "A.1"), "two");
  assert.throws(() => docSection(doc, "C"));
  assert.equal(moduleDoc("//! a\n//!\n//! b\nfn x() {}"), "a\n\nb");
});

test("intra-doc links keep their text, web links stay", () => {
  assert.equal(unlink("[`State::Corrupt`] and [`x`](crate::y) and [site](https://a.b)"), "`State::Corrupt` and `x` and [site](https://a.b)");
});

test("a layout reads put_uN, single bytes and slices, with consts", () => {
  const text = `const AT: usize = 8;\nconst END: usize = BLOCK - 4;\nconst BLOCK: usize = 64;\nimpl T {\n    pub fn encode(&self, out: &mut [u8]) {\n        put_u32(out, 0, MY_MAGIC);\n        out[4] = self.kind as u8;\n        out[AT..16].copy_from_slice(&self.id);\n        put_u32(\n            out,\n            16,\n            u32::try_from(self.bytes.len()).expect("x"),\n        );\n        put_u32(out, END, crc(&out[..END]));\n    }\n}\n`;
  assert.deepEqual(encodeLayout(text, "T"), [
    { at: 0, size: 4, name: "magic" },
    { at: 4, size: 1, name: "kind" },
    { at: 8, size: 8, name: "id" },
    { at: 16, size: 4, name: "len" },
    { at: 60, size: 4, name: "crc" },
  ]);
  assert.equal(constValue("const N: u64 = 2 * 1_024;", "N"), 2048);
});

test("the journal's layouts read from its code", () => {
  const format = repoFile("crates/moonpool-journal/src/format.rs");
  const record = encodeLayout(format, "Record");
  assert.equal(record.at(-1).at, 60);
  assert.equal(record.reduce((s, r) => s + r.size, 0), 4 + 1 + 1 + 2 + 4 + 8 + 8 + 4 + 4 + 24 + 2 - 4 + 4);
  assert.deepEqual(record.map((r) => r.name), ["kind", "flags", "index", "len", "batch", "position", "offset", "entry_crc", "id", "count", "crc"]);
  const entry = encodeLayout(format, "EntryHeader");
  assert.deepEqual(entry.map((r) => r.name), ["magic", "len", "batch", "position", "index", "count", "kind", "flags", "id", "tag", "crc"]);
  const header = encodeLayout(format, "SegmentHeader");
  assert.deepEqual(header.map((r) => r.name), ["magic", "version", "journal", "first_batch", "persist_blocks", "gap_blocks", "entry_blocks", "prev_batch", "crc"]);
  const meta = encodeLayout(format, "Meta");
  assert.equal(meta.at(-1).at, 4092);
  assert.ok(meta.some((r) => r.name === "bytes" && r.at === 80 && r.size === null));
  assert.deepEqual(implFields(format, "Default for Geometry", "default"), { persist_blocks: 7936, gap_blocks: 512, entry_blocks: 7936 });
});

test("an item's doc and its fields' docs read from the code", async () => {
  const { itemDoc, fieldDocs } = await import("./rustdoc.mjs");
  const text = "/// One record.\n///\n/// ```text\n/// 0 kind\n/// ```\n#[derive(Clone)]\npub(crate) struct Record {\n    /// Its kind.\n    pub kind: u8,\n    pub flags: u8,\n    /// Where the entry\n    /// starts.\n    pub offset: u32,\n}\n";
  assert.equal(itemDoc(text, "struct Record"), "One record.");
  assert.deepEqual(fieldDocs(text, "Record"), { kind: "Its kind.", offset: "Where the entry starts." });
  const format = repoFile("crates/moonpool-journal/src/format.rs");
  assert.match(itemDoc(format, "struct Record"), /persist record/);
  assert.ok(fieldDocs(format, "Meta").floor);
});

test("the record kinds of the journal read from its enum, with their bytes", async () => {
  const { variantDocs } = await import("./rustdoc.mjs");
  const text = "pub(crate) enum Kind {\n    /// An entry.\n    Put = 1,\n    /// A tombstone\n    /// over a range.\n    Clear = 2,\n    Other,\n}";
  assert.deepEqual(variantDocs(text, "Kind").map((k) => [k.name, k.value, k.doc]), [["Put", 1, "An entry."], ["Clear", 2, "A tombstone\nover a range."], ["Other", undefined, ""]]);
  const kinds = variantDocs(repoFile("crates/moonpool-journal/src/format.rs"), "Kind");
  assert.ok(kinds.length >= 3);
  assert.ok(kinds.every((k) => Number.isInteger(k.value) && k.doc.length > 0));
});
