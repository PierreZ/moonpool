# The Exploration Problem

<!-- toc -->

Running thousands of random seeds is a powerful technique. It finds race conditions, timing bugs, and failure-handling errors that hand-written tests never would. But there is a class of bugs it struggles with, and understanding why reveals the fundamental challenge of state-space exploration.

## The Sequential Luck Problem

Consider a distributed lock service with a subtle bug: the lock can be granted to two nodes simultaneously, but only when a specific failover event happens during a specific rollback window. The failover has a 1/1000 chance per simulation step. The rollback has a 1/1000 chance per step. For the bug to manifest, **both** must happen in the same run.

With independent random seeds, the probability of both events occurring is roughly 1/1,000,000. We need about a million simulation runs to have a decent chance of seeing it. Each run takes a few seconds. That is weeks of compute for one bug.

This is the **Sequential Luck Problem**: a bug requiring N unlikely events in sequence has probability that decreases **multiplicatively**. Two events at 1/1000 each need ~10^6 trials. Three events need ~10^9. The search space grows exponentially with the depth of the required sequence.

Now consider what happens with checkpoint-and-branch. We run seeds until one of them triggers the failover (about 1000 runs). At that moment, we snapshot the simulation and spawn new timelines from the failover point with different randomness. Each of those new timelines has a 1/1000 chance of hitting the rollback window. We need about 1000 branch timelines, plus the ~1000 root seeds to reach the failover. Total: roughly 2000 timelines instead of 1,000,000.

The improvement is not incremental. It changes the complexity from **multiplicative** (p1 * p2) to **additive** (p1 + p2). For three-event bugs, the difference is even more dramatic: ~3000 timelines instead of ~10^9.

## State Space Shapes

Not all exploration problems are alike. Antithesis, through their work running NES games with autonomous testing, discovered that state spaces have **shapes** that require fundamentally different approaches.

### Breadth-Dominated: The Zelda Problem

The Legend of Zelda (1986) is an open-world game with 128 overworld screens, 230+ dungeon rooms, 25 prerequisite items, and multiple weapons. The state space is **wide**: there are many things to discover, and they do not need to happen in a strict sequence. The challenge is not getting past a single barrier but covering a large surface area.

Antithesis solved Zelda with `SOMETIMES_EACH`, which equalizes exploration across all discovered states. Visit screen #47? Good. Now visit screen #48 with the same frequency. The exploration engine spreads its budget across the breadth of the space, ensuring no region is neglected.

In moonpool terms, this maps to `assert_sometimes_each!` assertions that track coverage across identity values. A distributed database might use this to ensure all partition ranges see similar test traffic, or all node roles get exercised equally.

### Depth-Dominated: The Gradius Insight

Gradius (1985) is a side-scrolling shooter that auto-scrolls. The player cannot go back. Progress is measured almost entirely by **survival time**. Antithesis tried a complex strategy tracking powerups, weapons, ship position, and score. Then they deleted all of it and ran with a minimal strategy: maximize time since power-on, tracking only 3 bytes of game memory (single-player mode, pause state, death animation).

The minimal strategy beat the entire game.

This revealed that for depth-dominated problems, the **platform infrastructure** (save/restore, checkpoint-and-branch) matters more than domain-specific strategy. The agent's gameplay was alien: it burned powerups on speed, clipped through walls, hid from bosses until they timed out, and hammered the pause button. But it worked because the platform kept branching from the deepest point reached.

For simulation testing, this means that sometimes the right approach is a minimal liveness assertion ("the system is still making progress") combined with aggressive checkpoint-and-branch, rather than elaborate state tracking.

### Barriers: The Castlevania Trap

Castlevania's Stage 6 has stompers: pillars that descend and crush the player. The exploration engine tracked Simon's position on a 32-pixel grid and tried to equalize coverage across grid cells. It cruised through the first five stages. Then it got stuck.

The problem was **doomed states**. The best-known exemplar for the critical grid cells was Simon standing under a descending stomper. That state is irrecoverable: no sequence of future inputs can save him. The explorer kept restarting from this doomed state and making zero progress.

Two fixes unlocked progress. First, refining the grid from 32-pixel to 16-pixel cells created safe zones **between** the stompers where non-doomed exemplars could be established. Second, adding the stomper position to the state tuple distinguished "under a high stomper" from "under a low stomper."

The lesson for simulation testing: when exploration gets stuck, the fix is either **better input distribution** (how chaos events are generated) or **better output interpretation** (what signals guide exploration). Heatmaps, assertion coverage reports, and sometimes-assertion hit rates are the diagnostic tools that tell you which one to adjust.

### Continuous Optimization: The Metroid Economy

Metroid (1986) presented the hardest challenge. The exploration engine could reach everywhere accessible without missiles. But red doors require 5 missiles to open, and the engine kept spending missiles on enemies (because it helped short-term exploration) rather than hoarding them for progression.

The naive solutions failed. Adding missile count to the state tuple caused state explosion (position times missile count is too many combinations). Requiring 5+ missiles was too restrictive (some areas need you to spend them).

The solution was **continuous optimization**: decouple the optimization objective from the exploration objective. Explore all positions, but **prefer** states with more missiles and health. When a better-resourced path to a state is found, propagate the improvement through all downstream states.

This generalizes beyond games. In distributed systems testing, the analogues are memory usage (prefer states with lower memory to catch leaks), queue depth (prefer states with deeper queues to find overflow bugs), and latency (prefer states with higher latency to find timeout bugs).

## Exploration as Resource Allocation

The NES examples reveal a deeper truth: exploration is fundamentally a **resource allocation** problem, not just a randomness problem.

We have a finite compute budget. We can spend it on breadth (more root seeds, wider coverage) or depth (more branches from interesting states, deeper sequences). We can spread it evenly across all splitpoints or concentrate it on the most productive ones. We can run one seed with massive energy or many seeds with moderate energy.

Every one of these decisions affects what bugs we find. A breadth-heavy approach misses deep sequential bugs. A depth-heavy approach misses bugs in unexplored regions. Even allocation wastes budget on barren splitpoints. Concentrated allocation might miss productive ones entirely.

The next four chapters describe how moonpool makes these allocation decisions automatically. We start with the basic mechanism (fork at discovery), add resource limits (energy budgets), make the allocation adaptive (batch-based forking with early termination), and finally show how multiple seeds explore genuinely different regions of the state space.

The goal is to find bugs that random testing cannot reach, using the same compute budget. And as we will see, the results are dramatic.
