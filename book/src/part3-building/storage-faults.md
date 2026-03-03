# Storage Faults

<!-- toc -->

- TigerBeetle-inspired storage fault patterns
- Read corruption: return wrong data on read
- Write corruption: write wrong data to disk
- Torn writes: partial write (crash mid-operation)
- Sync failures: `sync_all()` fails, data not persisted
- Misdirected reads/writes: operation targets wrong offset
- IOPS and bandwidth simulation: realistic timing
- The step-loop pattern: storage operations return `Poll::Pending`, require simulation stepping
- Code example: the required `while !handle.is_finished()` loop with `sim.step()` and `yield_now()`
