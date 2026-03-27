---
description: |
  Moonpool provider traits: TimeProvider, TaskProvider, NetworkProvider, RandomProvider, StorageProvider.
  TRIGGER when: replacing direct tokio calls with provider equivalents, choosing which provider trait to use, or writing code that needs time/task/network/random/storage in moonpool.
  DO NOT TRIGGER when: not working on moonpool simulation code.
---

# Using Providers

## When to Use This Skill

Invoke when:
- Writing code that needs time, task spawning, networking, randomness, or storage
- Choosing which provider trait to use for a given operation
- Replacing a direct tokio call with its provider equivalent

## Quick Reference

| Instead of | Use | Provider |
|------------|-----|----------|
| `tokio::time::sleep(d)` | `time.sleep(d).await` | `TimeProvider` |
| `tokio::time::timeout(d, f)` | `time.timeout(d, f).await` | `TimeProvider` |
| `Instant::now()` | `time.now()` (scheduling) / `time.timer()` (app logic) | `TimeProvider` |
| `tokio::spawn()` | `task_provider.spawn_task(name, f)` | `TaskProvider` |
| `tokio::net::TcpStream::connect()` | `network.connect(addr).await` | `NetworkProvider` |
| `tokio::net::TcpListener::bind()` | `network.bind(addr).await` | `NetworkProvider` |
| `rand::thread_rng()` | `random.random()` / `random.random_range()` | `RandomProvider` |
| `tokio::fs::File::open()` | `storage.open(path, opts).await` | `StorageProvider` |

Access all five via the `Providers` bundle: `providers.time()`, `providers.network()`, `providers.task()`, `providers.random()`, `providers.storage()`.

In simulation contexts (Process/Workload), use `ctx.time()`, `ctx.network()`, `ctx.random()`.

## Book Chapters

- `book/src/part2-foundations/04-provider-pattern.md` — the pattern and why it exists
- `book/src/part2-foundations/05-providers-quickstart.md` — quick start: swapping implementations
- `book/src/part2-foundations/06-providers-deepdive.md` — deep dive: design rationale
- `book/src/part2-foundations/07-provider-traits.md` — full trait definitions for all five providers

## Source

Provider traits defined in `moonpool-core/src/providers/`.
