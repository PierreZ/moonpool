// The techniques of the site (#414 (website makeover), site map), in the
// order of the index. `path` is set when the page exists: the index, the
// home page and the top bar link only to a page that exists.
export const TECHNIQUES = [
  { name: "One thread, one clock, one seed", path: "techniques/determinism/", line: "Run a seed twice and compare the two runs." },
  { name: "Same code in production and simulation", path: "techniques/same-code/", line: "One function, two sets of providers." },
  { name: "Network faults", path: "techniques/network/", line: "Cut a link, clog it, close a connection." },
  { name: "Storage faults", path: "techniques/storage/", line: "What a crash does to each sector after the last sync." },
  { name: "Reboots", path: "techniques/reboots/", line: "A process starts again with fresh memory." },
  { name: "Swarm testing", path: "techniques/swarm/", line: "Each seed draws its own set of faults." },
  { name: "Assertions that must fire", path: "techniques/assertions/", line: "The coverage fills seed after seed." },
  { name: "Multiverse exploration", path: "techniques/exploration/", line: "The run forks at each discovery." },
  { name: "From a red seed to the bug", path: "techniques/debugging/", line: "Replay, then walk back to the first wrong event." },
];
