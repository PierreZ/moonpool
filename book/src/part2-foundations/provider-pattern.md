# The Provider Pattern

<!-- toc -->

- The core idea: abstract all I/O behind traits, swap implementations for simulation vs production
- One line changes between "real system" and "simulated system"
- The `Providers` bundle trait: one type parameter carries all five providers
