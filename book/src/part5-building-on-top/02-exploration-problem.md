# The Exploration Problem

<!-- toc -->

Thousands of random seeds find race conditions, timing bugs and broken error paths that hand-written tests never reach. One class of bugs resists them. To see why, we need to look at how probabilities compose.

## The Sequential Luck Problem

Take a distributed lock service with a subtle bug. The lock can be granted to two nodes at once, but only when a failover happens inside a rollback window. The failover has a 1 in 1000 chance per step. The rollback has a 1 in 1000 chance per step. The bug needs **both** in one run.

With independent seeds, the chance of both events in one run is about 1 in 1,000,000. We need about a million runs for a fair chance to see it. At a few seconds per run, that is weeks of compute for one bug.

This is the **Sequential Luck Problem**. A bug that needs N rare events in sequence has a probability that shrinks **multiplicatively**. Two events at 1 in 1000 need about 10^6 trials. Three need about 10^9. The search space grows exponentially with the depth of the sequence.

Now replay and continue. We run seeds until one of them triggers the failover, about 1000 runs. At that moment we keep the replay prefix and schedule new timelines that reproduce the failover, then continue with different randomness. Each continuation has a 1 in 1000 chance to hit the rollback window. We need about 1000 continuations. Total: about 2000 timelines instead of 1,000,000.

The change is not incremental. The cost goes from **multiplicative** (p1 * p2) to **additive** (p1 + p2). For a three-event bug, about 3000 timelines replace 10^9.

The explorer's own test scenario shows the same arithmetic. The floor ladder in `crates/moonpool-explorer/src/simulations.rs` has 8 floors behind a 1 in 6 gate each. Brute force reaches the top floor once in about 1.7 million runs (6^-8). The explorer climbs it within a 2000-run budget, in-process and with workers.

## State Space Shapes

Not every exploration problem has the same shape. Antithesis learned this by running NES games under its platform, and each game taught a different lesson.

### Breadth-Dominated: The Zelda Problem

The Legend of Zelda (1986) is an open world: 128 overworld screens, more than 230 dungeon rooms, 25 prerequisite items. The state space is **wide**. There are many things to discover, and they do not need to happen in a strict order. The challenge is coverage of a large surface, not one hard barrier.

Antithesis solved Zelda with `SOMETIMES_EACH`, which equalizes exploration across every discovered state. Visit screen 47. Good. Now visit screen 48 as often. The budget spreads across the breadth of the space, so no region is neglected.

In moonpool this is `assert_sometimes_each!`, which tracks coverage per identity value. A database can use it to make sure every partition range sees traffic, or every node role gets exercised.

### Depth-Dominated: The Gradius Insight

Gradius (1985) is an auto-scrolling shooter. The player cannot go back. Progress is survival time. Antithesis first tracked powerups, weapons, ship position and score. Then they deleted all of it and tracked 3 bytes of game memory: single-player mode, pause state, death animation. The objective became "maximize time since power-on".

The minimal strategy beat the whole game.

For depth-dominated problems, the **platform** (checkpoint and branch) matters more than domain strategy. The agent's play was alien: it burned powerups on speed, clipped through walls, hid from bosses until they timed out. It worked because the platform kept branching from the deepest point reached.

For simulation testing this means: a minimal progress watermark (`assert_sometimes_greater_than!(level, 0, "dungeon level reached")`) plus aggressive branching can beat elaborate state tracking.

### Barriers: The Castlevania Trap

Castlevania's stage 6 has stompers, pillars that descend and crush the player. The engine tracked Simon's position on a 32-pixel grid and equalized coverage across cells. It cruised through five stages. Then it stuck.

The problem was **doomed states**. The best exemplar for the critical cells was Simon standing under a descending stomper. No future input saves him. The explorer kept restarting from that doomed state and made no progress.

Two fixes unlocked it. A finer 16-pixel grid created safe cells **between** the stompers. Adding the stomper position to the state tuple told "under a high stomper" from "under a low stomper".

The lesson: when exploration stalls, fix the **input distribution** (how chaos is generated) or the **output interpretation** (which signals guide the search). Coverage reports and sometimes-assertion hit rates tell you which one to adjust. Moonpool's answer to doomed exemplars is in [Exemplars and Continuations](./05-exemplars-continuations.md).

### Continuous Optimization: The Metroid Economy

Metroid (1986) was the hardest. The engine reached everything accessible without missiles. Red doors need 5 missiles, and the engine kept spending missiles on enemies because that helped short-term exploration.

Naive fixes failed. Missile count in the state tuple caused state explosion. Requiring 5 or more missiles was too strict, because some areas need you to spend them.

The solution was **continuous optimization**: decouple the optimization objective from the exploration objective. Explore every position, but **prefer** states with more missiles and health. When a better-resourced path to a state appears, propagate it downstream.

This generalizes to systems. Prefer states with lower memory to find leaks, deeper queues to find overflow bugs, higher latency to find timeout bugs. In moonpool the quality keys of `assert_sometimes_each!` carry this signal.

## Exploration as Resource Allocation

The NES examples show that exploration is a **resource allocation** problem, not only a randomness problem.

We have a finite compute budget. We can spend it on breadth (more root seeds) or depth (more continuations from interesting states). We can spread it evenly across known states or concentrate it on the most promising ones.

Each choice decides which bugs we find. Breadth misses deep sequential bugs. Depth misses unexplored regions. Even allocation wastes budget on doomed states. Concentrated allocation can skip productive ones.

The next three chapters describe moonpool's policy. It is simple on purpose: a productive run produces a bounded number of children, an unproductive run produces none. We start with the frontier controller, then the bounded worker pool, then the exemplar store and continuation scheduling that keep pushing the frontier deeper.
