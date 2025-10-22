//! Moonpool: Location-Transparent Distributed Actor System
//!
//! Moonpool provides a distributed actor system inspired by Microsoft Orleans,
//! built on the moonpool-foundation deterministic simulation framework.
//!
//! # Core Concepts
//!
//! - **Location Transparency**: Actors accessed by ID, not physical node location
//! - **Automatic Activation**: First message triggers actor creation on-demand
//! - **Single-Threaded Execution**: Messages processed sequentially per actor
//! - **Distributed Directory**: Tracks actor locations across nodes
//! - **Typed State Persistence**: `ActorState<T>` wrapper with automatic serialization
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use moonpool::prelude::*;
//!
//! #[derive(Debug, Clone, Serialize, Deserialize)]
//! struct BankAccountState {
//!     balance: u64,
//! }
//!
//! struct BankAccountActor {
//!     actor_id: ActorId,
//!     state: ActorState<BankAccountState>,
//! }
//!
//! #[async_trait(?Send)]
//! impl Actor for BankAccountActor {
//!     type State = BankAccountState;
//!
//!     fn actor_id(&self) -> &ActorId {
//!         &self.actor_id
//!     }
//!
//!     async fn on_activate(&mut self, state: Option<BankAccountState>) -> Result<()> {
//!         // Framework loads and deserializes state automatically
//!         let initial_state = state.unwrap_or(BankAccountState { balance: 0 });
//!         // Initialize ActorState wrapper
//!         Ok(())
//!     }
//!
//!     async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<()> {
//!         Ok(())
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     // Start actor runtime with namespace
//!     let runtime = ActorRuntime::builder()
//!         .namespace("dev")
//!         .listen_addr("127.0.0.1:5000")
//!         .build()
//!         .await?;
//!
//!     // Get actor reference (namespace "dev" automatically applied)
//!     let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
//!
//!     // Send messages
//!     alice.call(DepositRequest { amount: 100 }).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                  ActorRuntime                           │
//! │  (namespace, listen_addr, directory, storage)           │
//! └─────────────┬──────────────────────────┬────────────────┘
//!               │                          │
//!               ├──────────────┐           ├──────────────┐
//!               ▼              ▼           ▼              ▼
//!        ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐
//!        │ ActorCatalog│  │ MessageBus│  │ Directory │  │  Storage  │
//!        │ (local     │  │ (routing  │  │ (location)│  │ (persist) │
//!        │  registry) │  │  +        │  │           │  │           │
//!        │            │  │correlation)│  │           │  │           │
//!        └─────┬──────┘  └───────────┘  └───────────┘  └───────────┘
//!              │
//!              ▼
//!        ┌───────────────────┐
//!        │  ActorContext<A>  │
//!        │  (state, queue,   │
//!        │   lifecycle)      │
//!        └───────────────────┘
//! ```
//!
//! # Testing
//!
//! Moonpool is built for deterministic simulation testing with Buggify:
//!
//! ```rust,no_run
//! use moonpool_foundation::simulation::SimulationBuilder;
//!
//! async fn bank_account_workload(
//!     random: SimRandomProvider,
//!     network: SimNetworkProvider,
//!     time: SimTimeProvider,
//!     task_provider: TokioTaskProvider,
//!     topology: WorkloadTopology,
//! ) -> SimulationResult<SimulationMetrics> {
//!     // Create actor runtime with simulated providers
//!     let runtime = ActorRuntime::with_providers(
//!         "test",
//!         network,
//!         time,
//!         task_provider,
//!     ).await?;
//!
//!     // Run test workload...
//!     Ok(SimulationMetrics::default())
//! }
//! ```

// Module declarations
pub mod actor;
pub mod directory;
pub mod error;
pub mod messaging;
pub mod runtime;
pub mod storage;

// Re-export prelude for convenience
pub mod prelude;
