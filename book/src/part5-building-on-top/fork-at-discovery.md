# Fork at Discovery

<!-- toc -->

- The mechanism: `fork()` at the OS level when an assertion fires for the first time
- Parent waits on child, child explores from the discovery point with a new seed
- Sequential fork tree: each child can fork again, building a tree of timelines
- Shared memory (`mmap MAP_SHARED | MAP_ANONYMOUS`): parent and children share coverage data
- Coverage bitmap (8192 bits): tracks which code paths have been taken
- VirginMap: union of all bitmaps across the multiverse
- `has_new_bits()`: did this child discover something the parent hadn't seen?
- Assertion interaction: Sometimes assertions trigger forks, Always assertions don't
- Bug recipes: `"count@seed -> count@seed"` format for reproducing specific timelines
