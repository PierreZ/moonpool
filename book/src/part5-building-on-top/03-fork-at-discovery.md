# Fork at Discovery

<!-- toc -->

Now we know **why** checkpoint-and-branch matters. Let us see **how** it works in moonpool. The mechanism is surprisingly simple: Unix `fork()`, shared memory, and a coverage bitmap. No serialization. No checkpointing to disk. No custom snapshot format. Just the operating system doing what it already does well.

## The Fork

When a `sometimes` assertion succeeds for the first time (or a numeric watermark improves, or a frontier advances), the explorer calls `fork()`. The operating system creates a child process that is an exact copy of the parent: same memory, same simulation state, same processes, same network buffers, same pending timers. Everything.

The child then **reseeds** its RNG with a deterministic new seed derived from the parent seed, the assertion name, and the child index:

```text
child_seed = FNV-1a(parent_seed + mark_name + child_index)
```

From this point forward, the child's simulation diverges. Every random decision (network latency, failure injection, timer jitter) follows a different path. But the starting state is identical to the parent's at the moment of discovery.

The parent waits for the child to finish, then spawns the next one. Children can themselves encounter new splitpoints and fork again, building a **tree of timelines**:

```text
Root (seed 42) ──────────┬── splitpoint at RNG #200
  RNG #1..#200           │
                         ├── Timeline A (new seed) ──────────── done
                         ├── Timeline B (new seed) ──┬──────── done
                         │                           │
                         │                    nested splitpoint!
                         │                           │
                         │                    ├── B1 ── done
                         │                    └── B2 ── BUG FOUND!
                         │
                         └── Timeline C (new seed) ──────────── done
```

Timeline B2 found the bug. It required two successive unlikely events: the one that triggered the first splitpoint in the root, and the one that triggered the nested splitpoint in B. No single random seed would have produced both.

## Why fork() Is Perfect for This

Using `fork()` for simulation snapshots has properties that are hard to beat:

**Zero-cost snapshots.** The child gets the parent's entire address space through copy-on-write (COW). No data is actually copied until one side writes to a page. For a simulation that mostly reads its state during the remaining execution, the overhead is minimal.

**No serialization.** We do not need to serialize and deserialize simulation state. The child inherits everything: Rc pointers, VecDeques, HashMaps, trait objects. This would be impossible with a checkpoint-to-disk approach for a Rust simulation with complex ownership graphs.

**Isolation by default.** The child cannot corrupt the parent's state (or vice versa) because they have separate address spaces. The only shared state is explicitly allocated in shared memory.

**Deterministic child seeds.** The child seed is computed from the parent seed, assertion name, and child index using FNV-1a hashing. This means the entire multiverse tree is deterministic: same root seed always produces the same tree.

## Shared Memory: The Communication Channel

Parent and child processes have separate address spaces, but they need to share some state: coverage data, assertion counters, energy budgets, bug recipes. All of this lives in memory allocated with `mmap(MAP_SHARED | MAP_ANONYMOUS)`:

```text
Parent process memory:
+----------------------------------------------------+
|  Simulation state (processes, network, timers)        |  <- COW pages
|  RNG state (will be reseeded in child)             |  <- COW pages
+----------------------------------------------------+
|  MAP_SHARED memory:                                |  <- truly shared
|    - Assertion table (128 slots)                   |
|    - Coverage bitmap (1024 bytes)                  |
|    - Explored map (1024 bytes)                     |
|    - Energy budget                                 |
|    - Fork stats + bug recipe                       |
+----------------------------------------------------+
```

The `MAP_SHARED` memory is the **only** communication channel between parent and child. This is why the `moonpool-explorer` crate depends only on `libc`. No channels, no sockets, no IPC. Just memory that both processes can read and write.

All shared state uses atomic operations. Assertion slots use compare-and-swap for first-time discovery flags and `fetch_add` for counters. Energy budgets use `fetch_sub` with rollback on failure. The bug recipe uses a CAS to ensure only the first bug's recipe is recorded.

## The Coverage Bitmap

Each timeline gets a small bitmap: 1024 bytes, which is 8192 bit positions. When an assertion fires, it sets the bit at position `hash(assertion_name) % 8192`:

```text
Timeline A's bitmap:
byte 0          byte 1
[0 0 1 0 0 0 0 0] [0 0 0 0 0 1 0 0]
      ^                       ^
      bit 2                   bit 13
      (retry_fired)           (timeout_hit)
```

The **explored map** (also called the virgin map, borrowing from AFL fuzzer terminology) is the union of all bitmaps across all timelines. It lives in `MAP_SHARED` memory. After each child finishes, the parent merges the child's bitmap into the explored map with a bitwise OR:

```text
After Timeline A:   explored = 00100000 00000100 ...
After Timeline B:   explored = 00100000 00000110 ...
                                              ^
                                   Timeline B found bit 14!
                                   (that's new coverage)
```

The critical question after each child finishes is: **did this child find anything new?** The `has_new_bits()` check answers this with a single pass over both bitmaps:

```text
child    = 00000110     (bits 1, 2 set)
explored = 00000100     (bit 2 already known)
result   = child & !explored
         = 00000010     <- bit 1 is NEW!
```

If the result is nonzero, the child discovered at least one assertion path that no previous timeline had reached. This information drives the adaptive forking system we will see in a later chapter.

## Bug Recipes

When a child process detects an assertion violation, it exits with code 42 (the special "bug found" exit code). The parent detects this via `waitpid()`, records the bug in shared statistics, and saves the **recipe**: the complete sequence of splitpoints that led to the buggy timeline.

A recipe is a list of `(rng_call_count, child_seed)` pairs:

```text
[(151, 8837201), (80, 1293847), (42, 9918273)]
```

Formatted as a human-readable timeline string:

```text
151@8837201 -> 80@1293847 -> 42@9918273
```

To replay: start with the root seed, run until RNG call #151, reseed to 8837201. Run until call #80, reseed to 1293847. Run until call #42, reseed to 9918273. The simulation follows the exact same path to the bug.

This is the key benefit of deterministic simulation combined with fork-based exploration: bugs that required a tree of thousands of timelines to discover can be replayed as a single, straight-line execution guided by a recipe.

## What Triggers a Splitpoint

Not every assertion causes the explorer to fork. Only **discovery** assertions trigger splitpoints, and only when they discover something genuinely new:

| Assertion kind | Triggers a splitpoint when... |
|---|---|
| `assert_sometimes!` | The condition is true for the **first time** |
| `assert_reachable!` | The code path is reached for the **first time** |
| `assert_sometimes_gt!` | The observed value **beats** the previous watermark |
| `assert_sometimes_all!` | More conditions are simultaneously true **than ever** |
| `assert_sometimes_each!` | A new identity-key is seen, or quality improves |
| `assert_always!` | **Never** (invariant, not a discovery) |
| `assert_unreachable!` | **Never** (safety check, not a discovery) |

The "first time" guard uses a compare-and-swap on a `split_triggered` flag in shared memory. Once a mark has triggered, it will not trigger again for boolean assertions. Numeric watermarks and frontiers can trigger multiple times as they improve.

## The Process Model

Putting it all together, here is what happens when a simulation runs with exploration enabled:

1. The builder allocates shared memory and initializes the explorer context
2. The simulation runs with its root seed
3. When an assertion fires for the first time, the explorer:
   - Records the RNG call count (the splitpoint position)
   - Checks if it has energy budget remaining
   - Saves the parent's coverage bitmap
   - For each child timeline: clears the child bitmap, computes a deterministic child seed, calls `fork()`
   - The child reseeds, updates its depth and recipe, and **returns from the split function** to continue the simulation with new randomness
   - The parent waits, merges coverage, checks for bugs
4. After all children finish, the parent restores its bitmap and continues its own simulation
5. At the end, the builder collects statistics and bug recipes from shared memory

The child process does not know it was forked. From its perspective, the assertion fired, the simulation continued, and it eventually completed (or found a bug). The forking is invisible to the simulation code. This transparency is what makes exploration composable with the rest of the framework: no changes to processes, workloads, or transport code.

In the next chapter, we will see why the "checks if it has energy budget remaining" step is critical. Without it, a single productive splitpoint could fork an exponentially growing tree that consumes all available memory and CPU.
