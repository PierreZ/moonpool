// The crates page (#415 (new site with the book)). The list of crates and
// what each one is for come from the table of the root README.md; this file
// adds, for each crate, when to depend on it and its Guide chapter.
// `crateEntries` fails when the README and this file do not name the same
// crates, so a new crate cannot miss its entry.

/** @type {Record<string, { when: string, chapter: string }>} */
export const CRATE_NOTES = {
  moonpool: {
    when: "Start here. Use the default features in dev-dependencies, and `default-features = false, features = [\"tokio\"]` in the library that you ship.",
    chapter: "appendix/02-crate-map.md",
  },
  "moonpool-core": {
    when: "A library that takes the provider traits and must not pull in the simulator.",
    chapter: "part2-foundations/07-provider-traits.md",
  },
  "moonpool-sim": {
    when: "You run simulations without the facade, or you build for wasm32 with `default-features = false`.",
    chapter: "part4-integration/01-standalone-sim.md",
  },
  "moonpool-buggify": {
    when: "Shipped code that names fault sites with `hint!` and `buggify!`, with zero dependencies.",
    chapter: "part3-building/29-hints.md",
  },
  "moonpool-assertions": {
    when: "Shipped code that counts `sometimes` and `always` facts, with zero dependencies.",
    chapter: "part3-building/12-assertions.md",
  },
  "moonpool-explorer": {
    when: "Rarely directly: `moonpool-sim` uses it through its default `exploration` feature.",
    chapter: "part5-building-on-top/01-exploration.md",
  },
  "moonpool-hyper": {
    when: "HTTP/2, tonic or axum on the providers. The facade feature `hyper` gives the same code.",
    chapter: "part4-integration/08-hyper-stack.md",
  },
  "moonpool-prometheus": {
    when: "You want your Prometheus metrics in the simulation report. Facade feature `prometheus`.",
    chapter: "part3-building/25-metrics.md",
  },
  "moonpool-rpc": {
    when: "Typed calls between processes over the network provider. Facade feature `rpc`.",
    chapter: "part4-networking/02-rpc.md",
  },
  "moonpool-rpc-derive": {
    when: "Through `moonpool-rpc` with its `derive` feature, for the `#[service]` attribute.",
    chapter: "part4-networking/04-rpc-operations.md",
  },
  "moonpool-journal": {
    when: "A node needs a durable log that tells a crash from a corruption.",
    chapter: "part5-building-on-top/08-journal.md",
  },
  "moonpool-rpc-sim": {
    when: "Not published. Read it as an example of simulation campaigns and oracles.",
    chapter: "part4-networking/03-rpc-guarantees.md",
  },
  "moonpool-sim-examples": {
    when: "Not published. Copy from it: maze, dungeon, axum, tonic and journal simulations.",
    chapter: "appendix/08-xtask.md",
  },
  "moonpool-wasm-demo": {
    when: "Not published. The ping-pong run that this site plays, compiled to wasm.",
    chapter: "part4-integration/07-wasm.md",
  },
  "moonpool-calibrate": {
    when: "Not published. Measures the latency of a real disk and network for the simulator.",
    chapter: "appendix/07-calibration.md",
  },
  xtask: {
    when: "Not published. `cargo xtask sim run` builds and runs the simulation binaries.",
    chapter: "appendix/08-xtask.md",
  },
};

/** Join the README rows with the notes; fail when the two lists differ. */
export function crateEntries(rows, notes = CRATE_NOTES) {
  const names = rows.map((r) => r.name);
  const missing = names.filter((n) => !notes[n]);
  const extra = Object.keys(notes).filter((n) => !names.includes(n));
  if (missing.length || extra.length) {
    throw new Error(`crates page: no note for [${missing}], a note for no crate [${extra}] (src/lib/crates.mjs)`);
  }
  return rows.map((r) => ({ ...r, ...notes[r.name] }));
}
