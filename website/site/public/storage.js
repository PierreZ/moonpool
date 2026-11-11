// The live run of the Storage page (#417 (technique pages, part 1)): the
// player, and under it the log of each server as a row of sectors. Each
// state comes from the run's events (public/run-model.js sectorStates).
import { mount } from "./player.js";
import * as model from "./run-model.js";

const GLYPH = {
  written: ["w", "written, not synced"],
  durable: ["✓", "durable (synced)"],
  kept_old: ["↺", "crash: kept the old bytes"],
  kept_new: ["✓", "crash: kept the new bytes"],
  lost: ["✕", "crash: lost"],
  latent: ["≈", "crash: reads corrupt bytes"],
  shorn: ["/", "crash: half old, half new"],
  lost_synced: ["!", "a synced sector lost: the oracle fails the run"],
};

const root = document.getElementById("run");
const view = document.getElementById("sectors");
const oracle = document.getElementById("oracle");
const [recorded] = JSON.parse(document.getElementById("runs").textContent);
const player = mount(root, recorded, { label: "replicated register · crash damage on" });

function draw(cursor) {
  const run = player.run;
  const servers = model.lanes(run).filter((l) => l.role !== "workload");
  const written = run.events.filter((e) => e.name === "log_written").map((e) => e.fields.sector);
  const width = Math.max(1, ...written.map((s) => s + 1));
  const disks = model.sectorStates(run, cursor);
  view.replaceChildren(
    ...servers.map((s) => {
      const row = document.createElement("div");
      row.className = "disk";
      const name = document.createElement("span");
      name.className = "disk-name";
      name.textContent = s.ip + " register.log";
      row.append(name);
      const cells = document.createElement("span");
      cells.className = "cells";
      const disk = disks.get(s.ip) || new Map();
      for (let i = 0; i < width; i++) {
        const state = disk.get(i);
        const c = document.createElement("span");
        c.className = "cell" + (state ? " s-" + state : "");
        c.textContent = state ? GLYPH[state][0] : "";
        c.title = "sector " + i + ": " + (state ? GLYPH[state][1] : "never written");
        cells.append(c);
      }
      row.append(cells);
      return row;
    })
  );
  const lost = model.lostSynced(run);
  oracle.className = "oracle " + (lost.length ? "bad" : "ok");
  oracle.textContent = lost.length
    ? "✗ The oracle: a sector that a sync reported durable changed across a crash (" + lost.map((l) => l.ip + " sector " + l.sectors).join(", ") + ")."
    : "✓ The oracle: no sector that a sync reported durable changed across a crash in this run.";
}

const legend = document.getElementById("sector-legend");
legend.replaceChildren(
  ...Object.entries(GLYPH).map(([state, [g, text]]) => {
    const s = document.createElement("span");
    const c = document.createElement("span");
    c.className = "cell s-" + state;
    c.textContent = g;
    s.append(c, " " + text);
    return s;
  })
);
player.onCursor(draw);
draw(player.cursor);
