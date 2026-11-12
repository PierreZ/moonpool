// The live run of the home page (#416 (engine and player)): the recorded run
// of src/runs/home.json first, then the same seed in the engine (wasm, in a
// Web Worker). The address holds the seed (`?seed=256`). "Run a seed" opens
// the playground (#419 (playground)).
import { mount } from "./player.js";

const root = document.getElementById("run");
const runs = JSON.parse(document.getElementById("runs").textContent);
mount(root, runs[0], { label: "replicated register" });
