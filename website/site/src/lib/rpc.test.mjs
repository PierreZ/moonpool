import { test } from "node:test";
import assert from "node:assert/strict";
import { fillBytes, fixtures, fnBody, leValue, readerCode, shiftHeadings, variantDocs, writerLayout } from "./rustdoc.mjs";
import { admissionOrder, protoFields, statusTable, tagTable } from "./rpc.mjs";
import { repoFile } from "./source.mjs";

const rpc = (f) => repoFile("crates/moonpool-rpc/" + f);

test("rustdoc code reads as a reader sees it, and headings shift", () => {
  const md = "a\n```compile_fail\n# use x;\nfn f() {}\n```\n```text\n# kept\n```";
  assert.equal(readerCode(md), "a\n```rust\nfn f() {}\n```\n```text\n# kept\n```");
  assert.equal(shiftHeadings("# A\n```\n# b\n```\n## C", 2), "### A\n```\n# b\n```\n#### C");
});

test("a writer chain gives its fields in order, with conditions and nested matches", () => {
  const text = `fn encode(m: &M) -> Vec<u8> {
    let mut out = Writer::new();
    match m {
        M::A { id, body, flags, window } => {
            out.u8(KIND_A).u64(*id).u16(u16::try_from(body.len()).unwrap_or(u16::MAX));
            if flags & 1 != 0 {
                out.u64(*window);
            }
            out.bytes(body);
        }
        M::B { outcome } => {
            out.u8(KIND_B);
            match outcome {
                O::Ok { codec } => {
                    out.u8(0).u16(codec.get());
                }
                O::Err(e) => {
                    out.u8(e.code());
                }
            }
        }
    }
    out.0
}`;
  assert.deepEqual(writerLayout(text, "encode", "M::A {"), [
    { name: "kind", size: 1, when: undefined },
    { name: "id", size: 8, when: undefined },
    { name: "body_len", size: 2, when: undefined },
    { name: "window", size: 8, when: "flags & 1 != 0" },
    { name: "body", size: null, when: undefined },
  ]);
  assert.deepEqual(writerLayout(text, "encode", "M::B {", { branch: "O::Ok", literals: ["status"] }).map((r) => r.name), ["kind", "status", "codec"]);
  assert.deepEqual(writerLayout(text, "encode", "M::B {").map((r) => [r.name, r.size]), [["kind", 1], ["outcome", null]]);
  assert.match(fnBody(text, "encode"), /^\s*let mut out/);
});

test("a layout over a vector: lengths, conditions, and a vector that does not fit", () => {
  const rows = [{ name: "kind", size: 1 }, { name: "w", size: 2, when: "x" }, { name: "body_len", size: 2 }, { name: "body", size: null }, { name: "tail", size: null }];
  assert.deepEqual(fillBytes(rows, "01" + "0200" + "aabb" + "cc").map((f) => [f.name, f.at, f.size, f.bytes]), [
    ["kind", 0, 1, "01"], ["body_len", 1, 2, "0200"], ["body", 3, 2, "aabb"], ["tail", 5, 1, "cc"],
  ]);
  assert.equal(fillBytes(rows, "01" + "0700" + "0000" + "", { when: true })[1].value, "7");
  assert.throws(() => fillBytes([{ name: "a", size: 2 }], "010203"), /after the last field/);
  assert.throws(() => fillBytes([{ name: "a", size: 4 }], "0102"), /ends inside/);
  assert.equal(leValue("0100000000000080"), String(2n ** 63n + 1n));
  assert.deepEqual(fixtures("# c\na 01ff\nnot hex zz\n"), { a: "01ff" });
});

test("every golden frame of moonpool-rpc fits the layout its encoder writes", () => {
  const wire = rpc("src/protocol/wire.rs");
  const v1 = fixtures(rpc("tests/fixtures/wire-v1.txt"));
  const request = writerLayout(wire, "encode_message", "WireMessage::Request {");
  assert.deepEqual(request.map((r) => r.name).slice(0, 4), ["kind", "call_id", "incarnation", "token.index"]);
  assert.equal(fillBytes(request, v1["request-stream"], { when: true }).find((f) => f.name === "stream_window").value, "65536");
  assert.equal(fillBytes(request, v1.request).find((f) => f.name === "body").bytes, "0a026869");
  const end = fillBytes(writerLayout(wire, "encode_stream", "WireMessage::StreamEnd {"), v1["stream-end-failed"]);
  assert.deepEqual(end.map((f) => [f.name, f.value]), [["kind", "7"], ["call_id", "7"], ["items", "1"], ["status", "13"], ["detail", "5"]]);
  const hello = variantDocs(wire, "WireMessage").find((v) => v.name === "Hello");
  assert.ok(hello.fields.max_frame_bytes);
});

test("the statuses and the admission order read from the code", () => {
  const statuses = statusTable(rpc("src/protocol/wire.rs"), rpc("src/error.rs"));
  assert.deepEqual(statuses.map((s) => s.code), statuses.map((_, i) => i + 1));
  const by = Object.fromEntries(statuses.map((s) => [s.name, s]));
  assert.equal(by.BrokenPromise.execution, "MaybeExecuted");
  assert.equal(by.ReplyTooLarge.execution, "Executed");
  assert.equal(by.Overloaded.execution, "NotAdmitted");
  assert.equal(by.StaleIncarnation.terminal, true);
  assert.equal(by.MethodNotFound.terminal, false);
  assert.equal(by.ShuttingDown.since, 2);
  assert.equal(by.EndpointNotFound.since, 1);
  const order = admissionOrder(rpc("src/transport/mod.rs"));
  assert.equal(order[0], "ShuttingDown");
  assert.ok(order.indexOf("CodecMismatch") < order.indexOf("Unauthenticated"));
  assert.equal(order.at(-1), "Overloaded");
});

test("a reference reads field by field, with its tag table", () => {
  const refs = fixtures(rpc("tests/fixtures/references-v1.txt"));
  const fields = protoFields(refs.service);
  assert.deepEqual(fields.map((f) => f.tag), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
  assert.equal(fields.find((f) => f.tag === 11).value, "4500");
  assert.equal(fields.find((f) => f.tag === 10).value, "0a000101");
  assert.equal(fields.map((f) => f.bytes).join(""), refs.service);
  assert.throws(() => protoFields("0a05"), /past the end/);
  const tags = tagTable("| Tag | Field | Type |\n|---|---|---|\n| 1 | id | `uint32` |\n| 6, 7 | inc | `fixed64` |\n| 5..=6 | routing | |");
  assert.deepEqual(Object.keys(tags).map(Number), [1, 5, 6, 7]);
  assert.equal(tags[6].field, "inc");
  assert.equal(tags[5].field, "routing");
});
