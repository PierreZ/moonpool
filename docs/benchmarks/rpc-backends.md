# moonpool-rpc on Tokio and compio

The same `moonpool-rpc` runtime code on three setups, over loopback TCP.
Each setup has one server runtime and one client runtime:

- **tokio**: `TokioProviders`. Each side has a current-thread runtime on
  its own OS thread. Sockets are driven by epoll readiness, and each read
  and each write is its own syscall.
- **tokio-mt**: `TokioProviders`. Both sides run on one multi-thread
  runtime with 4 workers, as a Tokio service does.
- **compio**: `CompioProviders`. Each side has a compio runtime on its own
  OS thread. Socket I/O is submitted to `io_uring`.

The client keeps `callers` echo calls in flight at the same time. Each
call sends a body of the given size, and the reply carries the same body.
"CPU µs per call" is the process CPU time (user and system, both sides
together) divided by the number of calls.

## Reproduce

```text
cargo run --release -p moonpool-rpc --features compio-providers \
    --example backends -- --duration 5 --callers 1,32 --size 128
```

The example is `crates/moonpool-rpc/examples/backends.rs`. `--backends`
selects a subset of `tokio,tokio-mt,compio`.

## Environment

| | |
|---|---|
| kernel | 6.18.44-fc-v140 (a Firecracker guest) |
| CPU | 4 vCPUs |
| compio driver | `io_uring`: under strace, the compio run makes only `io_uring_enter` calls, and no `epoll_wait` |

**This is a cloud sandbox VM.** Use the numbers to compare the backends,
not as absolute numbers.

## Results

Two runs of 5 s for each cell. Where a cell has two numbers, they are
run 1 / run 2. Latencies are per call, in µs.

### 128 B

| backend | callers | calls/s | p50 | p99 | CPU µs per call |
|---|---|---|---|---|---|
| tokio | 1 | 13,317 / 13,777 | 68 / 66 | 163 / 159 | 80 / 78 |
| tokio-mt | 1 | 10,783 / 11,041 | 80 / 80 | 244 / 219 | 126 / 124 |
| compio | 1 | 10,848 / 10,954 | 82 / 81 | 197 / 201 | 101 / 101 |
| tokio | 32 | 105,276 / 90,982 | 268 / 323 | 784 / 847 | 17.2 / 19.7 |
| tokio-mt | 32 | 53,147 / 50,874 | 561 / 587 | 1,297 / 1,490 | 40.2 / 42.0 |
| compio | 32 | 134,595 / 126,818 | 215 / 226 | 457 / 556 | 8.7 / 8.9 |

### 4 KiB

| backend | callers | calls/s | p50 | p99 | CPU µs per call |
|---|---|---|---|---|---|
| tokio | 1 | 11,861 / 11,890 | 76 / 75 | 183 / 180 | 90 / 90 |
| tokio-mt | 1 | 10,299 / 10,120 | 83 / 87 | 227 / 237 | 139 / 139 |
| compio | 1 | 9,951 / 10,089 | 90 / 89 | 230 / 220 | 110 / 109 |
| tokio | 32 | 67,545 / 76,645 | 425 / 378 | 1,224 / 927 | 25.8 / 24.0 |
| tokio-mt | 32 | 39,832 / 40,035 | 754 / 757 | 1,819 / 1,714 | 50.8 / 51.2 |
| compio | 32 | 69,870 / 75,580 | 399 / 380 | 1,281 / 1,059 | 27.4 / 25.3 |

### 64 KiB

| backend | callers | calls/s | p50 | p99 | CPU µs per call |
|---|---|---|---|---|---|
| tokio | 1 | 4,758 / 4,639 | 198 / 203 | 414 / 454 | 228 / 234 |
| tokio-mt | 1 | 4,068 / 3,661 | 233 / 255 | 470 / 559 | 311 / 349 |
| compio | 1 | 3,670 / 3,331 | 249 / 273 | 583 / 644 | 291 / 319 |
| tokio | 32 | 6,557 / 5,990 | 4,654 / 5,132 | 9,272 / 11,190 | 245 / 246 |
| tokio-mt | 32 | 5,267 / 6,237 | 5,800 / 4,890 | 12,719 / 9,987 | 303 / 290 |
| compio | 32 | 8,023 / 7,733 | 3,706 / 3,851 | 7,253 / 7,696 | 235 / 243 |

### Syscalls

A separate 2 s run with 32 callers of 128 B, under `strace -f -c`.
strace slows both backends, so only the ratio is useful:

| backend | calls | syscalls | syscalls per call | main syscalls |
|---|---|---|---|---|
| tokio | about 15,100 | 35,619 | about 2.4 | `sendto` (30,912), `epoll_wait`, `recvfrom` |
| compio | about 92,800 | 28,772 | about 0.3 | `io_uring_enter` (30,423) |

## Findings

- **Many calls in flight, small messages: compio wins.** With 32 callers
  of 128 B, compio completes 20–40% more calls per second than Tokio
  current-thread, at half the CPU per call (about 9 µs against about 18
  µs). Tokio makes about one `sendto` per message. compio submits many
  sends and receives in one `io_uring_enter`.
- **One call at a time: Tokio current-thread is about 20% faster.** With
  nothing to batch, each call still needs a full round trip through the
  kernel, and compio's submit and complete path costs more than one
  `sendto` and one `recvfrom`.
- **Larger messages: the gap closes.** At 4 KiB the two are equal. At 64
  KiB, with 32 callers, compio is about 25% faster, but the copies cost
  more than the syscalls.
- **The multi-thread Tokio runtime is the slowest with many callers.** A
  runtime's I/O (accept, all the connections, framing, request decoding)
  runs in the one task that polls `RpcDriver::run`, so it uses one core at
  most. Its wake-ups also move between workers. With 32 callers it does
  about half the calls of Tokio current-thread. To use more cores, run one
  RPC runtime per core: on compio (thread-per-core) or on several Tokio
  runtimes.
