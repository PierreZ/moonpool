//! SpaceProcess and SpaceWorkload for the cargo hauling network.
//!
//! **Process** — boots a MoonpoolNode with Station and Ship actors.
//! **Workload** — drives cargo hauling operations with retry-based delivery.
//! No reconciliation needed: op_id dedup makes retries safe.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;

use crate::actors::{
    ActorStateStore, ClusterConfig, InMemoryDirectory, MembershipProvider, MoonpoolClient,
    MoonpoolNode, NodeConfig, SharedMembership,
};
use crate::{NetworkAddress, TimeProvider};
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{
    Process, SimContext, SimulationResult, assert_always, assert_reachable, assert_sometimes,
};

use super::actors::{
    CargoRequest, QueryShipRequest, QueryStationRequest, ShipActorImpl, ShipData, ShipRef,
    StationActorImpl, StationCargoRequest, StationRef, TravelRequest,
};
use super::model::{SPACE_MODEL_KEY, SpaceModel};
use super::operations::{SpaceOp, random_op};

/// Maximum retries for operations.
const MAX_RETRIES: usize = 3;

/// Maximum retries for initial seeding.
const SEED_RETRIES: usize = 5;

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

        let mut builder = MoonpoolNode::new(self.cluster.clone(), config)
            .with_providers(ctx.providers().clone())
            .register::<StationActorImpl>()
            .register::<ShipActorImpl>();
        builder = builder.with_state_handle(ctx.state().clone());
        let _node = builder
            .start()
            .await
            .map_err(|e| moonpool_sim::SimulationError::InvalidState(format!("node start: {e}")))?;

        ctx.shutdown().cancelled().await;

        Ok(())
    }
}

// ============================================================================
// SpaceWorkload — test driver
// ============================================================================

/// Cargo hauling workload: drives travel, load, and unload operations.
pub struct SpaceWorkload {
    /// Number of operations to execute per run.
    num_ops: usize,
    /// Station names.
    station_names: Vec<String>,
    /// Ship names.
    ship_names: Vec<String>,
    /// Shared cluster config.
    cluster: ClusterConfig,
    /// Directory (for state handle wiring).
    directory: Rc<InMemoryDirectory>,
    /// Membership (for state handle wiring).
    membership: Rc<SharedMembership>,
    /// State store for ship seeding.
    state_store: Rc<dyn ActorStateStore>,
    /// Reference model (always consistent).
    model: SpaceModel,
    /// Client runtime (populated in setup).
    client: Option<MoonpoolClient<SimProviders>>,
    /// Monotonic op_id counter.
    next_op_id: u64,
}

impl SpaceWorkload {
    /// Create a new space workload.
    pub fn new(
        num_ops: usize,
        stations: &[&str],
        ships: &[&str],
        cluster: ClusterConfig,
        directory: Rc<InMemoryDirectory>,
        membership: Rc<SharedMembership>,
        state_store: Rc<dyn ActorStateStore>,
    ) -> Self {
        Self {
            num_ops,
            station_names: stations.iter().map(|s| s.to_string()).collect(),
            ship_names: ships.iter().map(|s| s.to_string()).collect(),
            cluster,
            directory,
            membership,
            state_store,
            model: SpaceModel::new(),
            client: None,
            next_op_id: 1,
        }
    }

    fn alloc_op_id(&mut self) -> u64 {
        let id = self.next_op_id;
        self.next_op_id += 1;
        id
    }
}

/// Initial station inventory configuration.
struct StationSeed {
    name: &'static str,
    inventory: &'static [(&'static str, i64)],
}

/// Initial ship placement.
struct ShipSeed {
    name: &'static str,
    station: &'static str,
}

const STATION_SEEDS: &[StationSeed] = &[
    StationSeed {
        name: "alpha-mine",
        inventory: &[("ore", 500)],
    },
    StationSeed {
        name: "alpha-dock",
        inventory: &[("ore", 100), ("electronics", 100), ("fuel", 100)],
    },
    StationSeed {
        name: "beta-fab",
        inventory: &[("electronics", 500)],
    },
    StationSeed {
        name: "beta-dock",
        inventory: &[("ore", 100), ("electronics", 100), ("fuel", 100)],
    },
    StationSeed {
        name: "gamma-refinery",
        inventory: &[("fuel", 500)],
    },
    StationSeed {
        name: "gamma-dock",
        inventory: &[("ore", 100), ("electronics", 100), ("fuel", 100)],
    },
];

const SHIP_SEEDS: &[ShipSeed] = &[
    ShipSeed {
        name: "hauler-1",
        station: "alpha-mine",
    },
    ShipSeed {
        name: "hauler-2",
        station: "beta-fab",
    },
    ShipSeed {
        name: "hauler-3",
        station: "gamma-refinery",
    },
    ShipSeed {
        name: "hauler-4",
        station: "alpha-dock",
    },
];

#[async_trait(?Send)]
impl moonpool_sim::Workload for SpaceWorkload {
    fn name(&self) -> &str {
        "space-driver"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.model = SpaceModel::new();
        self.next_op_id = 1;

        self.directory.set_state_handle(ctx.state().clone());
        self.membership.set_state_handle(ctx.state().clone());

        let addr_str = format!("{}:4700", ctx.my_ip());
        let local_addr = NetworkAddress::parse(&addr_str).map_err(|e| {
            moonpool_sim::SimulationError::InvalidState(format!("invalid address: {e}"))
        })?;

        let client = MoonpoolClient::new(self.cluster.clone(), local_addr)
            .with_providers(ctx.providers().clone())
            .knows::<StationActorImpl>()
            .knows::<ShipActorImpl>()
            .start()
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("client start: {e}"))
            })?;

        self.client = Some(client);

        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let client = self.client.take().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("client not initialized".to_string())
        })?;

        // Seed stations with initial inventory
        for seed in STATION_SEEDS {
            for &(commodity, amount) in seed.inventory {
                let op_id = self.alloc_op_id();
                let actor_ref: StationRef<_> = client.actor_ref(seed.name.to_string());
                let mut seeded = false;
                for attempt in 0..SEED_RETRIES {
                    match actor_ref
                        .add_cargo(StationCargoRequest {
                            op_id,
                            commodity: commodity.to_string(),
                            amount,
                        })
                        .await
                    {
                        Ok(_) => {
                            seeded = true;
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(
                                station = seed.name,
                                commodity,
                                attempt,
                                error = %e,
                                "seed station failed"
                            );
                            if attempt < SEED_RETRIES - 1 {
                                let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                            }
                        }
                    }
                }
                if !seeded {
                    return Err(moonpool_sim::SimulationError::InvalidState(format!(
                        "seed failed for station {} commodity {commodity}",
                        seed.name
                    )));
                }
                self.model.seed_station(seed.name, commodity, amount);
            }
        }

        // Seed ships via state store (direct write, no RPC needed)
        for seed in SHIP_SEEDS {
            let data = ShipData {
                docked_at: seed.station.to_string(),
                cargo: Default::default(),
                completed_ops: Default::default(),
            };
            let json = serde_json::to_vec(&data).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("ship seed serialize: {e}"))
            })?;
            self.state_store
                .write_state("Ship", seed.name, json, None)
                .await
                .map_err(|e| {
                    moonpool_sim::SimulationError::InvalidState(format!("ship seed write: {e}"))
                })?;
            self.model.seed_ship(seed.name, seed.station);
        }

        // Publish initial model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        // Check process registration
        let expected = ctx.topology().all_process_ips().len();
        let active = self.membership.members().await.len();
        if active >= expected {
            assert_reachable!("all_processes_registered");
        }

        // Main operation loop
        for i in 0..self.num_ops {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Periodic yield to prevent false deadlock detection
            if i % 10 == 0 {
                let _ = ctx.time().sleep(Duration::from_nanos(1)).await;
            }

            let op = random_op(
                ctx.random(),
                &self.model,
                &self.station_names,
                &self.ship_names,
            );

            match op {
                SpaceOp::TravelTo { ship, station } => {
                    let ship_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    let mut succeeded = false;
                    for _ in 0..MAX_RETRIES {
                        match ship_ref
                            .travel_to(TravelRequest {
                                destination: station.clone(),
                            })
                            .await
                        {
                            Ok(resp) => {
                                assert_always!(
                                    resp.docked_at == station,
                                    "travel response matches",
                                    { "ship" => &ship, "expected" => &station, "actual" => &resp.docked_at }
                                );
                                self.model.travel_to(&ship, &station);
                                assert_sometimes!(true, "travel_succeeded");
                                succeeded = true;
                                break;
                            }
                            Err(e) => {
                                tracing::debug!(ship = %ship, error = %e, "travel_to failed, retrying");
                                let _ = ctx.time().sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    if !succeeded {
                        tracing::warn!(ship = %ship, "travel_to exhausted retries");
                    }
                }
                SpaceOp::LoadCargo {
                    ship,
                    commodity,
                    amount,
                } => {
                    // Check model preconditions
                    let loc = self.model.ship_location(&ship).unwrap_or("").to_string();
                    if !self.model.station_has(&loc, &commodity, amount) {
                        assert_sometimes!(true, "load_rejected_insufficient");
                        continue;
                    }

                    let op_id = self.alloc_op_id();
                    let ship_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    let mut succeeded = false;
                    for _ in 0..MAX_RETRIES {
                        match ship_ref
                            .load_cargo(CargoRequest {
                                op_id,
                                commodity: commodity.clone(),
                                amount,
                            })
                            .await
                        {
                            Ok(resp) => {
                                if resp.success {
                                    self.model.load_cargo(&ship, &loc, &commodity, amount);
                                    assert_sometimes!(true, "load_succeeded");
                                } else {
                                    assert_sometimes!(true, "load_rejected_insufficient");
                                }
                                succeeded = true;
                                break;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    ship = %ship, commodity = %commodity,
                                    error = %e, "load_cargo failed, retrying"
                                );
                                let _ = ctx.time().sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    if !succeeded {
                        tracing::warn!(ship = %ship, "load_cargo exhausted retries");
                    }
                }
                SpaceOp::UnloadCargo {
                    ship,
                    commodity,
                    amount,
                } => {
                    if !self.model.ship_has(&ship, &commodity, amount) {
                        assert_sometimes!(true, "unload_rejected_insufficient");
                        continue;
                    }

                    let loc = self.model.ship_location(&ship).unwrap_or("").to_string();
                    let op_id = self.alloc_op_id();
                    let ship_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    let mut succeeded = false;
                    for _ in 0..MAX_RETRIES {
                        match ship_ref
                            .unload_cargo(CargoRequest {
                                op_id,
                                commodity: commodity.clone(),
                                amount,
                            })
                            .await
                        {
                            Ok(resp) => {
                                if resp.success {
                                    self.model.unload_cargo(&ship, &loc, &commodity, amount);
                                    assert_sometimes!(true, "unload_succeeded");
                                } else {
                                    assert_sometimes!(true, "unload_rejected_insufficient");
                                }
                                succeeded = true;
                                break;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    ship = %ship, commodity = %commodity,
                                    error = %e, "unload_cargo failed, retrying"
                                );
                                let _ = ctx.time().sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    if !succeeded {
                        tracing::warn!(ship = %ship, "unload_cargo exhausted retries");
                    }
                }
                SpaceOp::QueryShip { ship } => {
                    let ship_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    match ship_ref.query_ship(QueryShipRequest {}).await {
                        Ok(resp) => {
                            let expected_loc =
                                self.model.ship_location(&ship).unwrap_or("").to_string();
                            assert_always!(
                                resp.docked_at == expected_loc,
                                "query ship location mismatch",
                                { "ship" => &ship, "actual" => &resp.docked_at, "expected" => &expected_loc }
                            );
                            let expected_cargo = self
                                .model
                                .ships
                                .get(&ship)
                                .map(|s| &s.cargo)
                                .cloned()
                                .unwrap_or_default();
                            assert_always!(
                                resp.cargo == expected_cargo,
                                "query ship cargo mismatch",
                                { "ship" => &ship, "actual" => format!("{:?}", resp.cargo), "expected" => format!("{:?}", expected_cargo) }
                            );
                        }
                        Err(e) => {
                            tracing::warn!(ship = %ship, error = %e, "query_ship failed");
                        }
                    }
                }
                SpaceOp::QueryStation { station } => {
                    let station_ref: StationRef<_> = client.actor_ref(station.clone());
                    match station_ref.query_state(QueryStationRequest {}).await {
                        Ok(resp) => {
                            let expected_inv = self
                                .model
                                .stations
                                .get(&station)
                                .map(|s| &s.inventory)
                                .cloned()
                                .unwrap_or_default();
                            assert_always!(
                                resp.inventory == expected_inv,
                                "query station inventory mismatch",
                                { "station" => &station, "actual" => format!("{:?}", resp.inventory), "expected" => format!("{:?}", expected_inv) }
                            );
                        }
                        Err(e) => {
                            tracing::warn!(station = %station, error = %e, "query_state failed");
                        }
                    }
                }
                SpaceOp::VerifyAll => {
                    let mut all_match = true;
                    for name in &self.station_names {
                        let station_ref: StationRef<_> = client.actor_ref(name.clone());
                        match station_ref.query_state(QueryStationRequest {}).await {
                            Ok(resp) => {
                                let expected_inv = self
                                    .model
                                    .stations
                                    .get(name)
                                    .map(|s| &s.inventory)
                                    .cloned()
                                    .unwrap_or_default();
                                assert_always!(
                                    resp.inventory == expected_inv,
                                    "verify: station inventory mismatch",
                                    { "station" => name, "actual" => format!("{:?}", resp.inventory), "expected" => format!("{:?}", expected_inv) }
                                );
                            }
                            Err(e) => {
                                tracing::warn!(station = %name, error = %e, "verify station failed");
                                all_match = false;
                            }
                        }
                    }
                    for name in &self.ship_names {
                        let ship_ref: ShipRef<_> = client.actor_ref(name.clone());
                        match ship_ref.query_ship(QueryShipRequest {}).await {
                            Ok(resp) => {
                                let expected_loc =
                                    self.model.ship_location(name).unwrap_or("").to_string();
                                assert_always!(
                                    resp.docked_at == expected_loc,
                                    "verify: ship location mismatch",
                                    { "ship" => name, "actual" => &resp.docked_at, "expected" => &expected_loc }
                                );
                                let expected_cargo = self
                                    .model
                                    .ships
                                    .get(name)
                                    .map(|s| &s.cargo)
                                    .cloned()
                                    .unwrap_or_default();
                                assert_always!(
                                    resp.cargo == expected_cargo,
                                    "verify: ship cargo mismatch",
                                    { "ship" => name, "actual" => format!("{:?}", resp.cargo), "expected" => format!("{:?}", expected_cargo) }
                                );
                            }
                            Err(e) => {
                                tracing::warn!(ship = %name, error = %e, "verify ship failed");
                                all_match = false;
                            }
                        }
                    }
                    if all_match {
                        assert_sometimes!(true, "verify_all_passed");
                    }
                }
                SpaceOp::SmallDelay => {
                    let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                }
            }

            // Publish updated model for invariant checking
            ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());
        }

        // Drop client
        drop(client);

        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Final cargo conservation
        for (commodity, &expected) in &self.model.total_cargo {
            let station_sum: i64 = self
                .model
                .stations
                .values()
                .map(|s| s.inventory.get(commodity).copied().unwrap_or(0))
                .sum();
            let ship_sum: i64 = self
                .model
                .ships
                .values()
                .map(|s| s.cargo.get(commodity).copied().unwrap_or(0))
                .sum();
            let actual = station_sum + ship_sum;
            assert_always!(actual == expected, "final cargo conservation", {
                "commodity" => commodity, "actual" => actual, "expected" => expected
            });
        }

        // Final non-negative check
        for (name, station) in &self.model.stations {
            for (commodity, &amount) in &station.inventory {
                assert_always!(amount >= 0, "final non-negative station inventory", {
                    "station" => name, "commodity" => commodity, "amount" => amount
                });
            }
        }
        for (name, ship) in &self.model.ships {
            for (commodity, &amount) in &ship.cargo {
                assert_always!(amount >= 0, "final non-negative ship cargo", {
                    "ship" => name, "commodity" => commodity, "amount" => amount
                });
            }
        }

        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        Ok(())
    }
}
