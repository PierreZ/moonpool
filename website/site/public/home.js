// The live run of the home page (#416 (engine and player)): the recorded run
// of src/runs/home.json first, then the same seed in the engine (wasm, in a
// Web Worker). The address holds the seed (`?seed=256`).
import { mount } from "./player.js";

const root = document.getElementById("run");
const runs = JSON.parse(document.getElementById("runs").textContent);
const player = mount(root, runs[0], { label: "replicated register" });

document.getElementById("hero-run").addEventListener("click", () => {
  root.scrollIntoView({ block: "center" });
  player.restart();
  player.play();
});
