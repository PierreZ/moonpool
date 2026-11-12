// The facts of the rpc extra page (#427 (rpc page)) that need more than one
// file of moonpool-rpc: the table of reply statuses and the order of the
// admission checks. Each one reads the code, so it follows the code.
import { fnBody, variantDocs } from "./rustdoc.mjs";

/**
 * The reply statuses, from `WireError` in `protocol/wire.rs` and
 * `RpcError::from_wire` in `error.rs`: `{ code, name, since, doc, execution,
 * reason, terminal }` by code. `execution` is what the status proves to the
 * caller, `terminal` whether the reference is dead after it.
 */
export function statusTable(wire, error) {
  const codes = [...fnBody(wire, "status_and_detail").matchAll(/Self::(\w+)(?:\s*\{[^}]*\})?\s*=>\s*\(\s*(\d+),/g)];
  if (!codes.length) throw new Error("status_and_detail maps no status");
  const since = /match self \{\s*([^=]+)=> (\d+),/.exec(fnBody(wire, "since"));
  if (!since) throw new Error("WireError::since names no version");
  const newer = new Set([...since[1].matchAll(/Self::(\w+)/g)].map((m) => m[1]));
  const docs = Object.fromEntries(variantDocs(wire, "WireError").map((v) => [v.name, v.doc]));
  const fromWire = fnBody(error, "from_wire");
  const arms = Object.fromEntries(
    [...fromWire.matchAll(/WireError::(\w+)[\s\S]*?(?=WireError::|$)/g)].map((m) => {
      const reason = /ErrorReason::(\w+)/.exec(m[0]);
      const execution = /Execution::(\w+)/.exec(m[0]);
      return [m[1], { reason: reason?.[1], execution: execution?.[1] ?? "NotAdmitted" }];
    }),
  );
  if (!/Self::not_admitted\(reason\)\s*$/.test(fromWire.trim())) throw new Error("from_wire no longer ends with not_admitted");
  const terminal = new Set([...fnBody(error, "is_terminal_for_reference").matchAll(/ErrorReason::(\w+)/g)].map((m) => m[1]));
  return codes.map(([, name, code]) => {
    const arm = arms[name];
    if (!arm) throw new Error(`from_wire does not map WireError::${name}`);
    if (docs[name] === undefined) throw new Error(`WireError::${name} has no doc`);
    return {
      code: Number(code),
      name,
      since: newer.has(name) ? Number(since[2]) : 1,
      doc: docs[name],
      execution: arm.execution,
      reason: arm.reason,
      terminal: terminal.has(arm.reason),
    };
  });
}

/**
 * The rejections of `fn admit` (`transport/mod.rs`) in the order the code
 * makes them, with the checks of `Registration::route` in their place: the
 * order in which a server refuses a request.
 */
export function admissionOrder(transport) {
  const admit = fnBody(transport, "admit");
  const route = fnBody(transport, "route");
  const at = admit.indexOf(".route(");
  if (at < 0) throw new Error("admit no longer routes");
  const names = (text) => [...text.matchAll(/WireError::(\w+)/g)].map((m) => m[1]);
  const order = [...names(admit.slice(0, at)), ...names(route), ...names(admit.slice(at))];
  return [...new Set(order)];
}

/**
 * The fields of a protobuf message in hex, in the order they are written:
 * `{ tag, type, at, bytes, value }`. `type` is the wire type (`varint`,
 * `fixed64`, `bytes`, `fixed32`); `bytes` holds the key and the value;
 * `value` is the number of a varint or a fixed field (little-endian), or the
 * hex of a length-delimited one.
 */
export function protoFields(hex) {
  const b = hex.match(/../g).map((x) => parseInt(x, 16));
  let i = 0;
  const varint = () => {
    let v = 0n;
    let shift = 0n;
    for (;;) {
      if (i >= b.length) throw new Error("a varint runs past the end");
      const byte = b[i++];
      v |= BigInt(byte & 0x7f) << shift;
      shift += 7n;
      if (!(byte & 0x80)) return v;
    }
  };
  const le = (n) => {
    let v = 0n;
    for (let k = n - 1; k >= 0; k--) v = (v << 8n) | BigInt(b[i + k]);
    i += n;
    return v;
  };
  const out = [];
  while (i < b.length) {
    const at = i;
    const key = varint();
    const tag = Number(key >> 3n);
    const wire = Number(key & 7n);
    let type;
    let value;
    if (wire === 0) [type, value] = ["varint", varint().toString()];
    else if (wire === 1) [type, value] = ["fixed64", le(8).toString()];
    else if (wire === 5) [type, value] = ["fixed32", le(4).toString()];
    else if (wire === 2) {
      const len = Number(varint());
      type = "bytes";
      value = hex.slice(i * 2, (i + len) * 2);
      i += len;
    } else throw new Error(`wire type ${wire} at byte ${at}`);
    if (i > b.length) throw new Error("a field runs past the end");
    out.push({ tag, type, at, bytes: hex.slice(at * 2, i * 2), value });
  }
  return out;
}

/**
 * The tag table of a reference's `# Encoding` doc (`| Tag | Field | Type |`),
 * by tag: `6, 7` names two tags and `5..=11` a range.
 */
export function tagTable(md) {
  const out = {};
  for (const m of md.matchAll(/^\| ([\d., =]+) \| ([^|]+) \|\s*([^|]*?)\s*\|$/gm)) {
    const field = m[2].trim();
    const type = m[3].trim().replace(/`/g, "");
    for (const part of m[1].split(",")) {
      const range = /(\d+)\.\.=(\d+)/.exec(part);
      const [lo, hi] = range ? [Number(range[1]), Number(range[2])] : [Number(part), Number(part)];
      for (let t = lo; t <= hi; t++) out[t] ??= { field, type };
    }
  }
  if (!Object.keys(out).length) throw new Error("no tag table in the doc");
  return out;
}
