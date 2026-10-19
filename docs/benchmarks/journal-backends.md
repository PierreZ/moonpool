# moonpool-journal on Tokio and compio

The same `moonpool-journal` code on two storage backends, on a real
filesystem:

- **Tokio**: `TokioStorageProvider` on a multi-thread runtime. Every file
  operation is a blocking syscall on Tokio's blocking pool, so each one hands
  off between threads. A durable write is a single `pwritev2(RWF_DSYNC)`.
- **compio**: `CompioStorageProvider` on one compio runtime, which is one
  thread. File operations are submitted to `io_uring`. A durable write is a
  write followed by an `fdatasync`.

Both runs use the same journal configuration and the same payloads. Spares
are prepared in the background on each backend's own task provider.
Neither backend was tuned.

## Reproduce

```text
cargo run --release -p moonpool-journal --features compio-providers \
    --example backends -- --duration 5
```

The example is `crates/moonpool-journal/examples/backends.rs`. It prints the
environment table and the result tables below as Markdown.

- `--backends`, `--workloads`, `--sizes`, `--writers` and `--fsync` each
  narrow the matrix.
- `--path` must be on the disk under test. The default is under `target/`.
- The process drops the page cache before each recovery run when it is
  allowed to write `/proc/sys/vm/drop_caches`. Otherwise the recovery table
  says it ran warm.

## Environment

| | |
|---|---|
| kernel | 6.18.44-fc-v139 (a Firecracker guest) |
| CPU | Intel Xeon @ 2.10GHz, 4 vCPUs |
| filesystem | ext4, the guest's root filesystem |
| device | `/dev/vda`, virtio-blk (vendor `0x1af4`). The guest reports it as rotational, which says nothing about the host's device. |
| compio driver | `IoUring`, not the polling fallback |
| direct I/O | available. `DirectIo::Required` opened every file. |

**This is a cloud sandbox VM, not a database host.** strace measured an
`fdatasync` at about 0.1 ms, so the host very likely absorbs flushes in a
write-back cache (this is inferred, not checked). On a real NVMe device with
power-loss protection, or worse on one without it, the sync is a larger part
of each commit, and the backends' difference shrinks in proportion.

## Workloads

- **Sequential**: one journal commits one record at a time and times each
  commit. Each run lasts 5 s, at 128 B, 4 KiB and 64 KiB.
- **Concurrent**: N journals commit 4 KiB records at the same time.
  `commit` takes `&mut self`, so N journals means N appends in flight. On
  Tokio, the runtime has min(N, cores) workers. On compio, all N run on its
  one thread.
- **Recovery**: a 256 MiB journal of 4 KiB records (65,536 entries in 4
  segments of the default geometry) is built with Tokio. Then the run does a
  `sync` and drops the page cache, and times `Journal::open`, which reads
  and checks every record, and then a full `replay(..)`. Each run rebuilds
  the journal.
- **I/O and fsync**:
  - Every workload runs buffered and with `DirectIo::Required`.
  - Each append workload runs with `Durability::Batched` (one sync per
    commit) and `Durability::Ordered` (two durable writes per commit).
- **Warm-up**: before the clock starts, each journal prepares its first
  segment inline and commits once onto it. This keeps the 64 MiB zero-fill
  of the first segment out of every run. Later rolls use the background
  spares, as in production.

The backends run in rotated order from one case to the next, so neither
always runs on a disk that the other just left behind.

## Results

There were two full runs of the matrix. Where a cell shows two numbers, they
are run 1 / run 2. Latencies are per commit, in µs.

### Sequential durable appends (one writer)

| record | I/O | fsync | Tokio appends/s | compio appends/s | compio / Tokio | Tokio p50 | compio p50 | Tokio p99.9 | compio p99.9 |
|---:|---|---|---:|---:|---:|---:|---:|---:|---:|
| 128 B | buffered | Batched | 1359 / 1357 | 1627 / 1580 | 1.20 / 1.16 | 653 / 645 | 546 / 591 | 7247 / 4943 | 4395 / 3055 |
| 128 B | direct | Batched | 1557 / 1578 | 1896 / 1844 | 1.22 / 1.17 | 539 / 530 | 450 / 473 | 4131 / 5343 | 3645 / 3133 |
| 4 KiB | buffered | Batched | 1361 / 1420 | 1448 / 1655 | 1.06 / 1.17 | 645 / 637 | 604 / 539 | 10303 / 4491 | 6235 / 3595 |
| 4 KiB | direct | Batched | 1597 / 1519 | 1900 / 1811 | 1.19 / 1.19 | 537 / 562 | 450 / 454 | 6059 / 4515 | 3663 / 4061 |
| 64 KiB | buffered | Batched | 1007 / 988 | 986 / 1078 | 0.98 / 1.09 | 815 / 821 | 693 / 707 | 45087 / 47775 | 62239 / 55839 |
| 64 KiB | direct | Batched | 852 / 908 | 1112 / 1142 | 1.31 / 1.26 | 946 / 897 | 684 / 659 | 55903 / 53791 | 60991 / 62111 |
| 128 B | buffered | Ordered | 1209 / 1299 | 1381 / 1477 | 1.14 / 1.14 | 716 / 679 | 660 / 618 | 4851 / 4103 | 4311 / 3929 |
| 128 B | direct | Ordered | 1278 / 1534 | 1793 / 1861 | 1.40 / 1.21 | 663 / 590 | 470 / 449 | 10655 / 3301 | 3299 / 5083 |
| 4 KiB | buffered | Ordered | 1176 / 1267 | 1109 / 1433 | 0.94 / 1.13 | 731 / 683 | 746 / 626 | 5595 / 10735 | 26319 / 3871 |
| 4 KiB | direct | Ordered | 1475 / 1368 | 1758 / 1432 | 1.19 / 1.05 | 596 / 649 | 470 / 526 | 3499 / 4655 | 4399 / 31503 |
| 64 KiB | buffered | Ordered | 885 / 824 | 887 / 804 | 1.00 / 0.98 | 877 / 990 | 858 / 957 | 62655 / 61503 | 63519 / 67519 |
| 64 KiB | direct | Ordered | 839 / 980 | 976 / 1022 | 1.16 / 1.04 | 956 / 790 | 715 / 676 | 59807 / 62175 | 66943 / 56447 |

### Concurrent writers (4 KiB records)

| writers | I/O | fsync | Tokio appends/s | compio appends/s | compio / Tokio | Tokio p50 | compio p50 | Tokio p99.9 | compio p99.9 |
|---:|---|---|---:|---:|---:|---:|---:|---:|---:|
| 4 | buffered | Batched | 2877 / 2879 | 2396 / 2766 | 0.83 / 0.96 | 1282 / 1273 | 1494 / 1302 | 6359 / 16879 | 67391 / 60767 |
| 4 | direct | Batched | 3227 / 3313 | 3040 / 3077 | 0.94 / 0.93 | 1083 / 1066 | 1135 / 1128 | 6575 / 5207 | 47775 / 48031 |
| 16 | buffered | Batched | 4270 / 3808 | 3184 / 2843 | 0.75 / 0.75 | 3733 / 3973 | 4787 / 5287 | 8147 / 35519 | 66239 / 26287 |
| 16 | direct | Batched | 4632 / 4586 | 3159 / 3535 | 0.68 / 0.77 | 3323 / 3357 | 4707 / 4331 | 29855 / 12943 | 44575 / 9791 |
| 4 | buffered | Ordered | 3087 / 2898 | 2294 / 2563 | 0.74 / 0.88 | 1142 / 1216 | 1502 / 1359 | 5519 / 7023 | 65855 / 60735 |
| 4 | direct | Ordered | 3155 / 3029 | 2945 / 2991 | 0.93 / 0.99 | 1105 / 1142 | 1166 / 1159 | 8239 / 26575 | 40639 / 54687 |
| 16 | buffered | Ordered | 3922 / 3853 | 2334 / 2247 | 0.60 / 0.58 | 3943 / 4019 | 6175 / 6603 | 10303 / 12687 | 61407 / 37279 |
| 16 | direct | Ordered | 4468 / 4004 | 3588 / 3194 | 0.80 / 0.80 | 3449 / 3805 | 4259 / 4707 | 20383 / 25391 | 12999 / 42047 |

The p99 columns are in the raw output of the example. They follow p50. For
example, the 4 KiB direct batched sequential case had a Tokio p99 of
1455 / 1737 µs and a compio p99 of 1220 / 1414 µs.

### Recovery (256 MiB, cold page cache)

There were three runs per cell, each on a freshly built journal. Each cell
shows the median and then the slowest run. The table has two sweeps with the
backend order swapped.

| backend | I/O | open ms, sweep 1 | open ms, sweep 2 | replay ms, sweep 1 | replay ms, sweep 2 |
|---|---|---:|---:|---:|---:|
| Tokio | buffered | 655 / 1519 | 371 / 450 | 1288 / 1367 | 1169 / 1303 |
| compio | buffered | 677 / 810 | 857 / 1299 | 1340 / 1499 | 1073 / 1282 |
| Tokio | direct | 511 / 525 | 454 / 500 | 421 / 421 | 408 / 428 |
| compio | direct | 535 / 567 | 457 / 464 | 394 / 400 | 377 / 451 |

### Syscalls per append

`strace -f -c` was run over a 3 s sequential 4 KiB run per backend (buffered
and direct, Batched and Ordered, about 7,400 Tokio and 11,500 compio
appends). Tracing slows both backends, so only the counts are meaningful.

| backend | file I/O syscalls / append | other syscalls / append | total / append |
|---|---:|---:|---:|
| Tokio | 2.5 (`pwritev2`, `pwrite64`, `fdatasync`) | about 15 (`futex` 9.8, `write` 2.5, `epoll_wait` 2.5) | about 17 |
| compio | 0 | 3.5 (`io_uring_enter`) | about 3.5 |

The `futex`, `epoll_wait` and `write` calls are Tokio's handoff of each file
operation to a blocking-pool thread and of its result back. The `write`
calls are very likely the runtime's wake-up of the driver (this is inferred).
compio makes all its file I/O through the ring.

## Conclusion

- **One writer: compio commits faster at small records, by about 15–20% in most cases.** The measured ratios range from 0.94 to 1.40.
  - Its median latency is 50–200 µs lower.
  - The gap is the blocking-pool handoff, which compio does not have. Per
    append, compio makes about 3.5 syscalls and Tokio about 17.
  - At 64 KiB, the two are equal in buffered mode. Bandwidth and the sync
    dominate there.
  - With direct I/O, compio leads in every case, by 4–40%.
- **Many writers: Tokio wins, and compio's tail is worse.**
  - At 16 journals, Tokio commits 25–72% more appends/s.
  - At 4 journals with buffered I/O, compio's p99.9 is 60–67 ms against
    Tokio's 6–17 ms.
  - The comparison is fair to the defaults but **not** to a thread-per-core
    design. Tokio spreads the work over 4 workers plus its blocking pool,
    while compio runs all 16 journals on one thread. A compio deployment
    would shard the journals over one runtime per core, and this bench does
    not measure that.
  - The cause of compio's tail is unexplained. One likely cause is buffered
    `io_uring` writes punted to io-wq workers and queued behind an
    `fdatasync` (this is inferred, not checked).
- **Recovery: no measurable difference.**
  - Open and replay of 256 MiB stay within run-to-run noise on both
    backends.
  - Direct I/O replays about 3× faster than buffered on a cold cache here
    (about 400 ms against 1.1–1.3 s).
- **Tails at 64 KiB (about 50–65 ms p99.9) are the same on both backends.**
  At about 60 MiB/s, a 5 s run fills several 64 MiB segments, so those
  commits include rolls while a spare is being zero-filled in the background
  (this is inferred from the rate, not counted).

## Caveats

- The VM's write-back cache very likely hides most of the cost of an fsync.
  On a real device, the sync takes a larger part of each commit, and the
  per-commit savings of compio shrink in proportion.
- The runs are 5 s each, two of them, on a shared 4-vCPU host. Differences
  under about 10% are within run-to-run noise: compare the run 1 and run 2
  columns.
- Recovery needs a fresh journal per run and a `sync` before the page cache
  is dropped. An earlier version of this bench reused one journal and did
  not `sync`, and then whichever backend ran first looked 2–3× faster at
  buffered replay. The table above does not have that problem.
- `/proc/self/io` was not used to count syscalls. It does not count
  `io_uring` submissions, and on Tokio its counts did not scale with the
  number of appends. Use strace instead.

## Easy wins

These were not applied, so the tables above stay untuned.

- **compio durable write in one submission.** `CompioStorageFile::write_at_durable_aligned`
  is a write and then an `fdatasync`: two ring round trips. Tokio uses one
  `pwritev2(RWF_DSYNC)`. A write with `RWF_DSYNC` in the SQE, or a linked
  write and fsync, would remove one round trip from every `Ordered` commit.
- **compio on more than one core.** Run one runtime per core and pin each
  journal to one of them. This is the deployment compio is built for, and
  the concurrent workload does not use it.
- **Tokio.** No easy win was found. The handoff to the blocking pool is how
  `tokio::fs` works.
