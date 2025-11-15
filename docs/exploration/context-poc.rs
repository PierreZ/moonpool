// Proof of Concept: Context Object Pattern
// This file demonstrates how the refactor would work in practice

use std::time::Duration;

// ============================================================================
// CURRENT APPROACH - Provider Traits (Complex)
// ============================================================================

mod current {
    use super::*;

    // Separate provider traits
    pub trait NetworkProvider: Clone {
        async fn connect(&self, addr: &str);
    }

    pub trait TimeProvider: Clone {
        async fn sleep(&self, dur: Duration);
    }

    pub trait TaskProvider: Clone {
        fn spawn(&self, name: &str, f: impl std::future::Future<Output = ()> + 'static);
    }

    // PROBLEM: Every struct needs 3+ generic parameters
    pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
        network: N,
        time: T,
        task: TP,
        destination: String,
    }

    impl<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> Peer<N, T, TP> {
        pub fn new(network: N, time: T, task: TP, destination: String) -> Self {
            Self { network, time, task, destination }
        }

        pub async fn send(&self, data: Vec<u8>) {
            // Access providers individually
            self.time.sleep(Duration::from_millis(100)).await;
            self.network.connect(&self.destination).await;
            // Complex to use
        }
    }

    // PROBLEM: Actor also needs all 3 generic parameters
    pub struct PingPongActor<
        N: NetworkProvider + Clone + 'static,
        T: TimeProvider + Clone + 'static,
        TP: TaskProvider + Clone + 'static,
    > {
        network: N,
        time: T,
        task: TP,
        peer: Peer<N, T, TP>,  // Must pass all generics through!
    }

    // PROBLEM: Construction is verbose
    pub fn create_actor<N, T, TP>(
        network: N,
        time: T,
        task: TP,
    ) -> PingPongActor<N, T, TP>
    where
        N: NetworkProvider + Clone + 'static,
        T: TimeProvider + Clone + 'static,
        TP: TaskProvider + Clone + 'static,
    {
        let peer = Peer::new(
            network.clone(),
            time.clone(),
            task.clone(),
            "127.0.0.1:8080".to_string(),
        );

        PingPongActor {
            network,
            time,
            task,
            peer,
        }
    }
}

// ============================================================================
// PROPOSED APPROACH - Context Object (Simple)
// ============================================================================

mod proposed {
    use super::*;

    // Provider traits stay the same
    pub trait NetworkProvider: Clone {
        async fn connect(&self, addr: &str);
    }

    pub trait TimeProvider: Clone {
        async fn sleep(&self, dur: Duration);
    }

    pub trait TaskProvider: Clone {
        fn spawn(&self, name: &str, f: impl std::future::Future<Output = ()> + 'static);
    }

    // NEW: Single Context struct that bundles all providers
    #[derive(Clone)]
    pub struct Ctx<N, T, TP> {
        pub network: N,
        pub time: T,
        pub task: TP,
    }

    // Concrete provider implementations
    #[derive(Clone)]
    pub struct SimNetworkProvider;
    #[derive(Clone)]
    pub struct SimTimeProvider;
    #[derive(Clone)]
    pub struct SimTaskProvider;

    impl NetworkProvider for SimNetworkProvider {
        async fn connect(&self, _addr: &str) {
            // Simulation implementation
        }
    }

    impl TimeProvider for SimTimeProvider {
        async fn sleep(&self, _dur: Duration) {
            // Simulation implementation
        }
    }

    impl TaskProvider for SimTaskProvider {
        fn spawn(&self, _name: &str, _f: impl std::future::Future<Output = ()> + 'static) {
            // Simulation implementation
        }
    }

    // Type aliases for convenience
    pub type SimCtx = Ctx<SimNetworkProvider, SimTimeProvider, SimTaskProvider>;

    // SOLUTION: Only 1 generic parameter!
    pub struct Peer<C> {
        ctx: C,
        destination: String,
    }

    impl<C> Peer<C>
    where
        C: Clone,
    {
        pub fn new(ctx: C, destination: String) -> Self {
            Self { ctx, destination }
        }
    }

    // For concrete usage, no generics needed!
    impl Peer<SimCtx> {
        pub async fn send(&self, _data: Vec<u8>) {
            // Clean access through context
            self.ctx.time.sleep(Duration::from_millis(100)).await;
            self.ctx.network.connect(&self.destination).await;
        }
    }

    // SOLUTION: Actor is much simpler
    pub struct PingPongActor {
        ctx: SimCtx,  // Concrete type!
        peer: Peer<SimCtx>,
    }

    // SOLUTION: Construction is clean
    pub fn create_actor(ctx: SimCtx) -> PingPongActor {
        let peer = Peer::new(
            ctx.clone(),
            "127.0.0.1:8080".to_string(),
        );

        PingPongActor { ctx, peer }
    }

    // Example: Creating a context
    pub fn example_usage() {
        let ctx = SimCtx {
            network: SimNetworkProvider,
            time: SimTimeProvider,
            task: SimTaskProvider,
        };

        // Pass single context instead of 3 separate providers
        let _actor = create_actor(ctx);
    }
}

// ============================================================================
// COMPARISON: Side by Side
// ============================================================================

mod comparison {
    use super::*;

    // BEFORE: 3 generic parameters, repeated trait bounds
    #[allow(dead_code)]
    pub struct OldPeer<N, T, TP>
    where
        N: Clone,
        T: Clone,
        TP: Clone,
    {
        network: N,
        time: T,
        task: TP,
    }

    // AFTER: 1 parameter, simpler bounds
    #[allow(dead_code)]
    pub struct NewPeer<C>
    where
        C: Clone,
    {
        ctx: C,
    }

    // BEFORE: Complex construction
    #[allow(dead_code)]
    pub fn old_create<N, T, TP>(network: N, time: T, task: TP) -> OldPeer<N, T, TP>
    where
        N: Clone,
        T: Clone,
        TP: Clone,
    {
        OldPeer { network, time, task }
    }

    // AFTER: Simple construction
    #[allow(dead_code)]
    pub fn new_create<C>(ctx: C) -> NewPeer<C>
    where
        C: Clone,
    {
        NewPeer { ctx }
    }
}

// ============================================================================
// ADVANCED: Context with Random Provider
// ============================================================================

mod with_random {
    use super::*;

    pub trait RandomProvider: Clone {
        fn random_u32(&self) -> u32;
    }

    // Context with optional random provider (default = unit type)
    #[derive(Clone)]
    pub struct Ctx<N, T, TP, R = ()> {
        pub network: N,
        pub time: T,
        pub task: TP,
        pub random: R,
    }

    #[derive(Clone)]
    pub struct SimNetworkProvider;
    #[derive(Clone)]
    pub struct SimTimeProvider;
    #[derive(Clone)]
    pub struct SimTaskProvider;
    #[derive(Clone)]
    pub struct SimRandomProvider;

    // Simulation context has random
    pub type SimCtx = Ctx<
        SimNetworkProvider,
        SimTimeProvider,
        SimTaskProvider,
        SimRandomProvider,
    >;

    // Production context doesn't need random (uses unit type)
    pub type TokioCtx = Ctx<
        SimNetworkProvider,  // Would be TokioNetworkProvider
        SimTimeProvider,     // Would be TokioTimeProvider
        SimTaskProvider,     // Would be TokioTaskProvider
        (),                  // No random in production
    >;

    // Example usage
    pub struct Actor {
        ctx: SimCtx,
    }

    impl Actor {
        pub fn new(ctx: SimCtx) -> Self {
            Self { ctx }
        }

        pub async fn work(&self) {
            // Access all providers through context
            self.ctx.time.sleep(Duration::from_millis(100)).await;
            self.ctx.network.connect("localhost").await;

            // Random is type-safe - only available in SimCtx
            let _n = self.ctx.random.random_u32();
        }
    }
}

// ============================================================================
// ALTERNATIVE: Enum-based Context (No Generics)
// ============================================================================

mod enum_approach {
    use super::*;

    // Concrete provider types
    #[derive(Clone)]
    pub struct SimNetworkProvider;
    #[derive(Clone)]
    pub struct SimTimeProvider;
    #[derive(Clone)]
    pub struct TokioNetworkProvider;
    #[derive(Clone)]
    pub struct TokioTimeProvider;

    #[derive(Clone)]
    pub struct SimCtx {
        pub network: SimNetworkProvider,
        pub time: SimTimeProvider,
    }

    #[derive(Clone)]
    pub struct TokioCtx {
        pub network: TokioNetworkProvider,
        pub time: TokioTimeProvider,
    }

    // Single enum that can be either
    #[derive(Clone)]
    pub enum Ctx {
        Sim(SimCtx),
        Tokio(TokioCtx),
    }

    // No generics at all!
    pub struct Peer {
        ctx: Ctx,  // Concrete type
        destination: String,
    }

    impl Peer {
        pub fn new(ctx: Ctx, destination: String) -> Self {
            Self { ctx, destination }
        }

        pub async fn send(&self, _data: Vec<u8>) {
            // Access through pattern matching or helper methods
            match &self.ctx {
                Ctx::Sim(sim) => {
                    sim.time.sleep(Duration::from_millis(100)).await;
                    // ...
                }
                Ctx::Tokio(tokio) => {
                    tokio.time.sleep(Duration::from_millis(100)).await;
                    // ...
                }
            }
        }
    }

    // Or with helper methods on Ctx
    impl Ctx {
        pub fn time(&self) -> &dyn TimeProvider {
            match self {
                Ctx::Sim(ctx) => &ctx.time as &dyn TimeProvider,
                Ctx::Tokio(ctx) => &ctx.time as &dyn TimeProvider,
            }
        }

        pub fn network(&self) -> &dyn NetworkProvider {
            match self {
                Ctx::Sim(ctx) => &ctx.network as &dyn NetworkProvider,
                Ctx::Tokio(ctx) => &ctx.network as &dyn NetworkProvider,
            }
        }
    }

    // Requires trait objects
    pub trait TimeProvider {
        fn sleep(&self, dur: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
    }

    pub trait NetworkProvider {
        fn connect(&self, addr: &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
    }

    impl TimeProvider for SimTimeProvider {
        fn sleep(&self, _dur: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
            Box::pin(async {})
        }
    }

    impl TimeProvider for TokioTimeProvider {
        fn sleep(&self, _dur: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
            Box::pin(async {})
        }
    }

    impl NetworkProvider for SimNetworkProvider {
        fn connect(&self, _addr: &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
            Box::pin(async {})
        }
    }

    impl NetworkProvider for TokioNetworkProvider {
        fn connect(&self, _addr: &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
            Box::pin(async {})
        }
    }

    // Example usage - completely generic-free!
    pub fn example() {
        let sim_ctx = Ctx::Sim(SimCtx {
            network: SimNetworkProvider,
            time: SimTimeProvider,
        });

        let _peer = Peer::new(sim_ctx, "127.0.0.1".to_string());
    }
}

// ============================================================================
// METRICS COMPARISON
// ============================================================================

/*
LINES OF CODE COMPARISON:

BEFORE (Provider Traits):
    - Peer definition: 6 lines (3 generic params + bounds)
    - Peer constructor: 8 lines (all params)
    - Actor definition: 8 lines (3 generic params + bounds)
    - Actor constructor: 15 lines (passing all providers)
    TOTAL: ~37 lines for basic setup

AFTER (Context Object):
    - Peer definition: 3 lines (1 generic param or concrete type)
    - Peer constructor: 3 lines (single ctx param)
    - Actor definition: 3 lines (concrete type)
    - Actor constructor: 5 lines (single ctx param)
    TOTAL: ~14 lines for basic setup

REDUCTION: 62% less code!

GENERIC PARAMETERS:
    - Before: 3-4 per struct
    - After: 0-1 per struct

TYPE COMPLEXITY:
    - Before: Peer<N, T, TP> where N: NP, T: TP, TP: TP
    - After: Peer<SimCtx> or just Peer

COMPILATION TIME:
    - Before: Monomorphization of every N×T×TP combination
    - After (with type aliases): Same monomorphization, but cleaner code
    - After (with enum): Less monomorphization, faster compile

RUNTIME PERFORMANCE:
    - Generic approach: Zero cost
    - Enum approach: Small enum tag check (~1 nanosecond)
    - Both negligible compared to I/O operations
*/

fn main() {
    println!("Context Object Pattern Proof of Concept");
    println!("See code comments for comparison details");
}
