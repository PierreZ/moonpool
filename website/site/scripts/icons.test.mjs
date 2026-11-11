import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { faviconSvg, ico, markInner, tokenColour } from "./icons.mjs";

const read = (p) => readFileSync(new URL(p, import.meta.url), "utf8");

test("favicon.svg is the mark of the top bar, in the colours of the tokens", () => {
  const css = read("../../shared/tokens.css");
  const svg = faviconSvg(markInner(read("../src/components/Mark.astro")), tokenColour(css, "accent"), tokenColour(css, "accent-fg"));
  assert.equal(read("../public/favicon.svg"), svg, "run node scripts/icons.mjs and commit public/");
  assert.throws(() => markInner("<svg></svg>"));
  assert.throws(() => tokenColour(css, "no-such-token"));
});

test("an ICO file lists each PNG with its size and its offset", () => {
  const a = Buffer.from("aaaa");
  const b = Buffer.from("bbbbbb");
  const out = ico([{ size: 16, png: a }, { size: 256, png: b }]);
  assert.equal(out.readUInt16LE(2), 1);
  assert.equal(out.readUInt16LE(4), 2);
  assert.equal(out.readUInt8(6), 16);
  assert.equal(out.readUInt8(22), 0);
  assert.equal(out.readUInt32LE(6 + 8), 4);
  assert.equal(out.readUInt32LE(6 + 12), 38);
  assert.equal(out.readUInt32LE(22 + 12), 42);
  assert.equal(out.subarray(38).toString(), "aaaabbbbbb");
});
