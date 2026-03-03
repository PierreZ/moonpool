# The Five Providers

<!-- toc -->

- `TimeProvider`: `sleep()`, `now()`, `timer()` (with clock drift in sim), `timeout()`
- `NetworkProvider`: `bind()`, `connect()` — associated types for TcpStream/TcpListener
- `TaskProvider`: `spawn_task()`, `yield_now()` — local spawning only
- `RandomProvider`: `random()`, `random_range()`, `random_ratio()`, `random_bool()`
- `StorageProvider`: `open()`, `exists()`, `delete()`, `rename()` — with `StorageFile` trait for file operations
- Production implementations: `TokioTimeProvider`, `TokioNetworkProvider`, etc.
- The `Providers` bundle: how `SimProviders` and `TokioProviders` package all five together
