//! SpaceProcess and SpaceWorkload for spacesim.
//!
//! **Process** — boots a MoonpoolNode with StationActorImpl on a server node.
//! **Workload** — drives deposits, withdrawals, and queries against the stations,
//! tracking a reference model for invariant checking.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;

use crate::actors::{ActorStateStore, ClusterConfig, InMemoryStateStore, MoonpoolNode, NodeConfig};
use crate::{NetworkAddress, RandomProvider, TimeProvider};
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{Process, SimContext, SimulationResult, assert_always, assert_sometimes};

use super::actors::{
    DepositCreditsRequest, QueryStateRequest, StationActorImpl, StationRef, WithdrawCreditsRequest,
};
use super::model::{SPACE_MODEL_KEY, SpaceModel};
use super::operations::{SpaceOp, random_op};

// ============================================================================
// SpaceProcess — system under test
// ============================================================================

/// Space station process: boots a MoonpoolNode and holds it alive.
pub struct SpaceProcess {
    cluster: ClusterConfig,
    state_store: Rc<dyn ActorStateStore>,
}

impl SpaceProcess {
    /// Create a new space process.
    pub fn new(cluster: ClusterConfig, state_store: Rc<dyn ActorStateStore>) -> Self {
        Self {
            cluster,
            state_store,
        }
    }
}

#[async_trait(?Send)]
impl Process for SpaceProcess {
    fn name(&self) -> &str {
        "station-node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let addr_str = format!("{}:4700", ctx.my_ip());
        let local_addr = NetworkAddress::parse(&addr_str).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("invalid address: {e}"))
        })?;

        let config = NodeConfig::builder()
            .address(local_addr)
            .state_store(self.state_store.clone())
            .build();

        let _node = MoonpoolNode::new(self.cluster.clone(), config)
            .with_providers(ctx.providers().clone())
            .register::<StationActorImpl>()
            .start()
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("node start: {e}")))?;

        // Hold alive until shutdown signal
        ctx.shutdown().cancelled().await;

        Ok(())
    }
}

// ============================================================================
// SpaceWorkload — test driver
// ============================================================================

/// Space economy workload: drives operations against station actors.
pub struct SpaceWorkload {
    /// Number of operations to execute per run.
    num_ops: usize,
    /// Station names to use.
    station_names: Vec<String>,
    /// Shared cluster config.
    cluster: ClusterConfig,
    /// Shared state store (concrete for `clear()`).
    state_store: Rc<InMemoryStateStore>,
    /// Reference model.
    model: SpaceModel,
    /// Actor runtime (populated in setup).
    node: Option<MoonpoolNode<SimProviders>>,
}

impl SpaceWorkload {
    /// Create a new space workload.
    pub fn new(
        num_ops: usize,
        stations: &[&str],
        cluster: ClusterConfig,
        state_store: Rc<InMemoryStateStore>,
    ) -> Self {
        Self {
            num_ops,
            station_names: stations.iter().map(|s| s.to_string()).collect(),
            cluster,
            state_store,
            model: SpaceModel::new(),
            node: None,
        }
    }
}

#[async_trait(?Send)]
impl moonpool_sim::Workload for SpaceWorkload {
    fn name(&self) -> &str {
        "space-driver"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Fresh state per iteration
        self.state_store.clear();
        self.model = SpaceModel::new();

        let addr_str = format!("{}:4700", ctx.my_ip());
        let local_addr = NetworkAddress::parse(&addr_str).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("invalid address: {e}"))
        })?;

        let config = NodeConfig::builder()
            .address(local_addr)
            .state_store(self.state_store.clone() as Rc<dyn ActorStateStore>)
            .build();

        let node = MoonpoolNode::new(self.cluster.clone(), config)
            .with_providers(ctx.providers().clone())
            .register::<StationActorImpl>()
            .start()
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("node start: {e}")))?;

        self.node = Some(node);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let node = self.node.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("node not initialized".to_string())
        })?;

        // Seed stations with initial credits
        for name in &self.station_names {
            let initial = ctx.random().random_range(10..100) as i64;
            let actor_ref: StationRef<_> = node.actor_ref(name.clone());
            match actor_ref
                .deposit_credits(DepositCreditsRequest { amount: initial })
                .await
            {
                Ok(_) => self.model.seed_station(name, initial),
                Err(e) => {
                    return Err(moonpool_sim::SimulationError::InvalidState(format!(
                        "initial deposit failed: {e}"
                    )));
                }
            }
        }

        // Publish initial model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        // Main operation loop
        for i in 0..self.num_ops {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Periodic sleep to generate simulation events and prevent false
            // deadlock detection.
            if i % 10 == 0 {
                let _ = ctx.time().sleep(Duration::from_nanos(1)).await;
            }

            let op = random_op(ctx.random(), &self.station_names);
            let node = self.node.as_ref().ok_or_else(|| {
                moonpool_sim::SimulationError::InvalidState("node not initialized".to_string())
            })?;

            match op {
                SpaceOp::Deposit { station, amount } => {
                    let actor_ref: StationRef<_> = node.actor_ref(station.clone());
                    match actor_ref
                        .deposit_credits(DepositCreditsRequest { amount })
                        .await
                    {
                        Ok(resp) => {
                            self.model.deposit(&station, amount);
                            let expected = self.model.station_credits(&station);
                            assert_always!(resp.credits == expected, "deposit credit mismatch");
                        }
                        Err(e) => {
                            tracing::warn!("deposit failed: {}", e);
                        }
                    }
                }
                SpaceOp::Withdraw { station, amount } => {
                    // Check model first — only issue withdrawals that should succeed
                    let can_withdraw = self.model.station_credits(&station) >= amount;
                    if !can_withdraw {
                        assert_sometimes!(true, "withdraw_rejected_insufficient");
                        continue;
                    }
                    let actor_ref: StationRef<_> = node.actor_ref(station.clone());
                    match actor_ref
                        .withdraw_credits(WithdrawCreditsRequest { amount })
                        .await
                    {
                        Ok(resp) => {
                            let withdrew = self.model.withdraw(&station, amount);
                            assert_always!(
                                withdrew,
                                "model withdraw should succeed when actor succeeded"
                            );
                            let expected = self.model.station_credits(&station);
                            assert_always!(resp.credits == expected, "withdraw credit mismatch");
                            assert_sometimes!(true, "withdraw_succeeded");
                        }
                        Err(e) => {
                            tracing::warn!("withdraw failed: {}", e);
                        }
                    }
                }
                SpaceOp::QueryState { station } => {
                    let actor_ref: StationRef<_> = node.actor_ref(station.clone());
                    match actor_ref.query_state(QueryStateRequest {}).await {
                        Ok(resp) => {
                            let expected = self.model.station_credits(&station);
                            assert_always!(resp.credits == expected, "query credit mismatch");
                        }
                        Err(e) => {
                            tracing::warn!("query_state failed: {}", e);
                        }
                    }
                }
                SpaceOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                }
            }

            // Publish updated model for invariant checking
            ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());
        }

        // Shutdown: drop node to trigger deactivation
        drop(self.node.take());

        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Final conservation check
        let sum = self.model.total_station_credits();
        assert_always!(sum == self.model.total_credits, "final credit conservation");

        // Final non-negative check
        for station in self.model.stations.values() {
            assert_always!(station.credits >= 0, "final non-negative station credits");
        }

        // Publish final model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        Ok(())
    }
}
