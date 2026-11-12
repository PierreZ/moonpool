// The extra pages of the site (#427 (rpc page), #428 (journal page)): one
// crate each, in depth. `path` is set when the page exists.
export const EXTRAS = [
  { name: "moonpool-journal, byte by byte", path: "extras/journal/", crate: "moonpool-journal", line: "The CLSTORE journal: its files, its records, its two commit protocols and how recovery tells a crash from corruption." },
  { name: "moonpool-rpc, call by call", path: "extras/rpc/", crate: "moonpool-rpc", line: "Typed RPC between dynamic endpoints: its frames, its call rules, admission and security." },
];
