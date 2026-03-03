# Attrition: Process Reboots

<!-- toc -->

- Automatic chaos for process lifecycle
- `Attrition` config: `max_dead`, `prob_graceful`, `prob_crash`, `prob_wipe`
- `RebootKind::Graceful`: signal cancellation token → grace period → force kill → restart
- `RebootKind::Crash`: immediate abort, connections drop instantly
- `RebootKind::CrashAndWipe`: crash + delete all storage
- Recovery delay: configurable range (default 1-10s) before process restarts
- `max_dead` constraint: never kill more processes than the system can tolerate
- Requires `.phases()` to be configured
