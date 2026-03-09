//! SpaceProcess and SpaceWorkload for spacesim.
//!
//! **Process** — boots a MoonpoolNode with StationActorImpl on a server node.
//! **Workload** — drives deposits, withdrawals, and queries against the stations,
//! tracking a reference model for invariant checking.

use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;

use crate::actors::{
    ActorStateStore, ClusterConfig, InMemoryDirectory, MembershipProvider, MoonpoolClient,
    MoonpoolNode, NodeConfig, SharedMembership,
};
use crate::{NetworkAddress, RandomProvider, TimeProvider};
use moonpool_sim::providers::SimProviders;
use moonpool_sim::{
    Process, SimContext, SimulationResult, assert_always, assert_reachable, assert_sometimes,
};

use super::actors::{
    AddCargoRequest, DepositCreditsRequest, QueryShipRequest, QueryStateRequest,
    RemoveCargoRequest, ShipActorImpl, ShipData, ShipRef, StationActorImpl, StationRef,
    TradeDirection, TradeRequest, WithdrawCreditsRequest,
};
use super::model::{SPACE_MODEL_KEY, SpaceModel};
use super::operations::{SpaceOp, random_op};

/// Maximum retries for reconciliation queries.
const RECONCILE_RETRIES: usize = 3;

/// Maximum retries for the end-of-run verification sweep.
const SWEEP_RETRIES: usize = 10;

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

        // Hold alive until shutdown signal
        ctx.shutdown().cancelled().await;

        Ok(())
    }
}

// ============================================================================
// SpaceWorkload — test driver
// ============================================================================

/// Space economy workload: drives operations against station and ship actors.
pub struct SpaceWorkload {
    /// Number of operations to execute per run.
    num_ops: usize,
    /// Station names to use.
    station_names: Vec<String>,
    /// Ship names to use.
    ship_names: Vec<String>,
    /// Shared cluster config.
    cluster: ClusterConfig,
    /// Concrete directory (for `set_state_handle`).
    directory: Rc<InMemoryDirectory>,
    /// Concrete membership (for `set_state_handle`).
    membership: Rc<SharedMembership>,
    /// State store for ship seeding.
    state_store: Rc<dyn ActorStateStore>,
    /// Reference model.
    model: SpaceModel,
    /// Client runtime (populated in setup).
    client: Option<MoonpoolClient<SimProviders>>,
}

impl SpaceWorkload {
    /// Try to reconcile a station with bounded retries. Returns true if reconciled.
    async fn try_reconcile_station(
        client: &MoonpoolClient<SimProviders>,
        model: &mut SpaceModel,
        station: &str,
        max_retries: usize,
    ) -> bool {
        let actor_ref: StationRef<_> = client.actor_ref(station.to_string());
        for attempt in 0..max_retries {
            match actor_ref.query_state(QueryStateRequest {}).await {
                Ok(resp) => {
                    model.reconcile(station, &resp);
                    tracing::info!(
                        station,
                        credits = resp.credits,
                        attempt,
                        "reconciled station"
                    );
                    return true;
                }
                Err(e) => {
                    tracing::debug!(station, attempt, error = %e, "reconcile query failed");
                }
            }
        }
        tracing::warn!(
            station,
            max_retries,
            "reconciliation failed, staying uncertain"
        );
        false
    }

    /// Try to reconcile a ship with bounded retries. Returns true if reconciled.
    async fn try_reconcile_ship(
        client: &MoonpoolClient<SimProviders>,
        model: &mut SpaceModel,
        ship: &str,
        max_retries: usize,
    ) -> bool {
        let actor_ref: ShipRef<_> = client.actor_ref(ship.to_string());
        for attempt in 0..max_retries {
            match actor_ref.query_ship(QueryShipRequest {}).await {
                Ok(resp) => {
                    model.reconcile_ship(ship, &resp);
                    tracing::info!(ship, credits = resp.credits, attempt, "reconciled ship");
                    return true;
                }
                Err(e) => {
                    tracing::debug!(ship, attempt, error = %e, "reconcile ship query failed");
                }
            }
        }
        tracing::warn!(
            ship,
            max_retries,
            "ship reconciliation failed, staying uncertain"
        );
        false
    }

    /// End-of-run verification sweep: reconcile all entities with retries.
    async fn verification_sweep(
        client: &MoonpoolClient<SimProviders>,
        model: &mut SpaceModel,
        station_names: &[String],
        ship_names: &[String],
        ctx: &SimContext,
    ) {
        // Brief sleep to let in-flight RPCs settle
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;

        for name in station_names {
            Self::try_reconcile_station(client, model, name, SWEEP_RETRIES).await;
        }
        for name in ship_names {
            Self::try_reconcile_ship(client, model, name, SWEEP_RETRIES).await;
        }
    }

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
        self.model = SpaceModel::new();

        // Wire state handles for invariant checking
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
        let client = self.client.as_ref().ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("client not initialized".to_string())
        })?;

        // Seed stations with initial credits (fault-tolerant)
        for name in &self.station_names {
            let initial = ctx.random().random_range(10..100) as i64;
            let actor_ref: StationRef<_> = client.actor_ref(name.clone());
            let mut seeded = false;
            for attempt in 0..SEED_RETRIES {
                match actor_ref
                    .deposit_credits(DepositCreditsRequest { amount: initial })
                    .await
                {
                    Ok(_) => {
                        self.model.seed_station(name, initial);
                        seeded = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(station = %name, attempt, error = %e, "seed deposit failed");
                        if attempt < SEED_RETRIES - 1 {
                            let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
            }
            if !seeded {
                return Err(moonpool_sim::SimulationError::InvalidState(format!(
                    "seed failed after {SEED_RETRIES} retries for station {name}"
                )));
            }
        }

        // Seed ships with initial credits via state store
        for name in &self.ship_names {
            let initial = ctx.random().random_range(50..200) as i64;
            let data = ShipData {
                credits: initial,
                cargo: Default::default(),
            };
            let json = serde_json::to_vec(&data).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("ship seed serialize: {e}"))
            })?;
            self.state_store
                .write_state("Ship", name, json, None)
                .await
                .map_err(|e| {
                    moonpool_sim::SimulationError::InvalidState(format!("ship seed write: {e}"))
                })?;
            self.model.seed_ship(name, initial);
        }

        // Publish initial model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        // Check process registration and actor distribution
        let expected = ctx.topology().all_process_ips().len();
        let active = self.membership.members().await.len();
        if active >= expected {
            assert_reachable!("all_processes_registered");
        }
        if active > 1 {
            assert_sometimes!(true, "cross_node_rpc");
        }

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

            let op = random_op(ctx.random(), &self.station_names, &self.ship_names);

            match op {
                SpaceOp::Deposit { station, amount } => {
                    let actor_ref: StationRef<_> = client.actor_ref(station.clone());
                    match actor_ref
                        .try_deposit_credits(DepositCreditsRequest { amount })
                        .await
                    {
                        Ok(resp) => {
                            if self.model.is_uncertain(&station) {
                                // Opportunistic reconciliation from successful response
                                self.model.reconcile(&station, &resp);
                            } else {
                                self.model.deposit(&station, amount);
                                let expected = self.model.station_credits(&station);
                                assert_always!(resp.credits == expected, "deposit credit mismatch", {
                                    "station" => &station, "actual" => resp.credits, "expected" => expected, "amount" => amount
                                });
                            }
                        }
                        Err(e) if e.is_maybe_delivered() => {
                            assert_sometimes!(true, "deposit_maybe_delivered");
                            self.model.mark_uncertain(&station);
                        }
                        Err(e) => {
                            tracing::warn!("deposit failed: {}", e);
                        }
                    }
                }
                SpaceOp::Withdraw { station, amount } => {
                    // Reconcile uncertain entity before precondition check
                    if self.model.is_uncertain(&station)
                        && !Self::try_reconcile_station(
                            client,
                            &mut self.model,
                            &station,
                            RECONCILE_RETRIES,
                        )
                        .await
                    {
                        continue; // Skip if can't reconcile
                    }
                    let can_withdraw = self.model.station_credits(&station) >= amount;
                    if !can_withdraw {
                        assert_sometimes!(true, "withdraw_rejected_insufficient");
                        continue;
                    }
                    let actor_ref: StationRef<_> = client.actor_ref(station.clone());
                    match actor_ref
                        .try_withdraw_credits(WithdrawCreditsRequest { amount })
                        .await
                    {
                        Ok(resp) => {
                            let withdrew = self.model.withdraw(&station, amount);
                            assert_always!(
                                withdrew,
                                "model withdraw should succeed when actor succeeded",
                                { "station" => &station, "amount" => amount }
                            );
                            let expected = self.model.station_credits(&station);
                            assert_always!(resp.credits == expected, "withdraw credit mismatch", {
                                "station" => &station, "actual" => resp.credits, "expected" => expected, "amount" => amount
                            });
                            assert_sometimes!(true, "withdraw_succeeded");
                        }
                        Err(e) if e.is_maybe_delivered() => {
                            assert_sometimes!(true, "withdraw_maybe_delivered");
                            self.model.mark_uncertain(&station);
                        }
                        Err(e) => {
                            tracing::warn!("withdraw failed: {}", e);
                        }
                    }
                }
                SpaceOp::QueryState { station } => {
                    let actor_ref: StationRef<_> = client.actor_ref(station.clone());
                    match actor_ref.query_state(QueryStateRequest {}).await {
                        Ok(resp) => {
                            if self.model.is_uncertain(&station) {
                                self.model.reconcile(&station, &resp);
                            } else {
                                let expected = self.model.station_credits(&station);
                                assert_always!(resp.credits == expected, "query credit mismatch", {
                                    "station" => &station, "actual" => resp.credits, "expected" => expected
                                });
                            }
                        }
                        Err(e) => {
                            tracing::warn!("query_state failed: {}", e);
                        }
                    }
                }
                SpaceOp::AddCargo {
                    station,
                    commodity,
                    amount,
                } => {
                    let actor_ref: StationRef<_> = client.actor_ref(station.clone());
                    match actor_ref
                        .try_add_cargo(AddCargoRequest {
                            commodity: commodity.clone(),
                            amount,
                        })
                        .await
                    {
                        Ok(resp) => {
                            if self.model.is_uncertain(&station) {
                                self.model.reconcile(&station, &resp);
                            } else {
                                self.model.add_cargo(&station, &commodity, amount);
                                let expected = self.model.station_cargo(&station, &commodity);
                                let actual = resp.inventory.get(&commodity).copied().unwrap_or(0);
                                assert_always!(actual == expected, "add_cargo inventory mismatch", {
                                    "station" => &station, "commodity" => &commodity, "actual" => actual, "expected" => expected, "amount" => amount
                                });
                            }
                        }
                        Err(e) if e.is_maybe_delivered() => {
                            assert_sometimes!(true, "add_cargo_maybe_delivered");
                            self.model.mark_uncertain(&station);
                        }
                        Err(e) => {
                            tracing::warn!("add_cargo failed: {}", e);
                        }
                    }
                }
                SpaceOp::RemoveCargo {
                    station,
                    commodity,
                    amount,
                } => {
                    if self.model.is_uncertain(&station)
                        && !Self::try_reconcile_station(
                            client,
                            &mut self.model,
                            &station,
                            RECONCILE_RETRIES,
                        )
                        .await
                    {
                        continue;
                    }
                    let can_remove = self.model.station_cargo(&station, &commodity) >= amount;
                    if !can_remove {
                        assert_sometimes!(true, "remove_cargo_rejected");
                        continue;
                    }
                    let actor_ref: StationRef<_> = client.actor_ref(station.clone());
                    match actor_ref
                        .try_remove_cargo(RemoveCargoRequest {
                            commodity: commodity.clone(),
                            amount,
                        })
                        .await
                    {
                        Ok(resp) => {
                            let removed = self.model.remove_cargo(&station, &commodity, amount);
                            assert_always!(
                                removed,
                                "model remove_cargo should succeed when actor succeeded",
                                { "station" => &station, "commodity" => &commodity, "amount" => amount }
                            );
                            let expected = self.model.station_cargo(&station, &commodity);
                            let actual = resp.inventory.get(&commodity).copied().unwrap_or(0);
                            assert_always!(actual == expected, "remove_cargo inventory mismatch", {
                                "station" => &station, "commodity" => &commodity, "actual" => actual, "expected" => expected
                            });
                            assert_sometimes!(true, "remove_cargo_succeeded");
                        }
                        Err(e) if e.is_maybe_delivered() => {
                            assert_sometimes!(true, "remove_cargo_maybe_delivered");
                            self.model.mark_uncertain(&station);
                        }
                        Err(e) => {
                            tracing::warn!("remove_cargo failed: {}", e);
                        }
                    }
                }
                SpaceOp::Trade {
                    ship,
                    station,
                    commodity,
                    amount,
                    price,
                    direction,
                } => {
                    // Reconcile uncertain entities before pre-checks
                    if self.model.is_ship_uncertain(&ship)
                        && !Self::try_reconcile_ship(
                            client,
                            &mut self.model,
                            &ship,
                            RECONCILE_RETRIES,
                        )
                        .await
                    {
                        continue;
                    }
                    if self.model.is_uncertain(&station)
                        && !Self::try_reconcile_station(
                            client,
                            &mut self.model,
                            &station,
                            RECONCILE_RETRIES,
                        )
                        .await
                    {
                        continue;
                    }

                    // Pre-condition checks against model
                    match &direction {
                        TradeDirection::Buy => {
                            if self.model.ship_credits(&ship) < price {
                                assert_sometimes!(true, "trade_insufficient_credits");
                                continue;
                            }
                            if self.model.station_cargo(&station, &commodity) < amount {
                                assert_sometimes!(true, "trade_insufficient_cargo");
                                continue;
                            }
                        }
                        TradeDirection::Sell => {
                            if self.model.ship_cargo(&ship, &commodity) < amount {
                                assert_sometimes!(true, "trade_insufficient_cargo");
                                continue;
                            }
                            if self.model.station_credits(&station) < price {
                                assert_sometimes!(true, "trade_insufficient_credits");
                                continue;
                            }
                        }
                    }

                    let actor_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    match actor_ref
                        .try_trade(TradeRequest {
                            station: station.clone(),
                            commodity: commodity.clone(),
                            amount,
                            price,
                            direction: direction.clone(),
                        })
                        .await
                    {
                        Ok(resp) => {
                            if resp.traded {
                                match direction {
                                    TradeDirection::Buy => {
                                        self.model
                                            .trade_buy(&ship, &station, &commodity, amount, price);
                                        assert_sometimes!(true, "trade_buy_succeeded");
                                    }
                                    TradeDirection::Sell => {
                                        self.model
                                            .trade_sell(&ship, &station, &commodity, amount, price);
                                        assert_sometimes!(true, "trade_sell_succeeded");
                                    }
                                }
                            }
                            // If !resp.traded, precondition failed inside actor (race)
                        }
                        Err(e) if e.is_maybe_delivered() => {
                            assert_sometimes!(true, "trade_maybe_delivered");
                            // Mark both entities uncertain — trade touches both
                            self.model.mark_ship_uncertain(&ship);
                            self.model.mark_uncertain(&station);
                        }
                        Err(e) => {
                            tracing::warn!("trade failed: {}", e);
                            self.model.mark_ship_uncertain(&ship);
                            self.model.mark_uncertain(&station);
                        }
                    }
                }
                SpaceOp::QueryShip { ship } => {
                    let actor_ref: ShipRef<_> = client.actor_ref(ship.clone());
                    match actor_ref.query_ship(QueryShipRequest {}).await {
                        Ok(resp) => {
                            if self.model.is_ship_uncertain(&ship) {
                                self.model.reconcile_ship(&ship, &resp);
                            } else {
                                let expected = self.model.ship_credits(&ship);
                                assert_always!(
                                    resp.credits == expected,
                                    "query ship credit mismatch",
                                    { "ship" => &ship, "actual" => resp.credits, "expected" => expected }
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("query_ship failed: {}", e);
                        }
                    }
                }
                SpaceOp::VerifyAll => {
                    // Under chaos, VerifyAll is a reconciliation sweep
                    let mut all_match = true;
                    for name in &self.station_names {
                        let actor_ref: StationRef<_> = client.actor_ref(name.clone());
                        match actor_ref.query_state(QueryStateRequest {}).await {
                            Ok(resp) => {
                                if self.model.is_uncertain(name) {
                                    self.model.reconcile(name, &resp);
                                } else {
                                    let expected_credits = self.model.station_credits(name);
                                    assert_always!(
                                        resp.credits == expected_credits,
                                        "verify: credit mismatch",
                                        { "station" => name, "actual" => resp.credits, "expected" => expected_credits }
                                    );
                                    let expected_inv = self
                                        .model
                                        .stations
                                        .get(name)
                                        .map(|s| &s.inventory)
                                        .cloned()
                                        .unwrap_or_default();
                                    assert_always!(
                                        resp.inventory == expected_inv,
                                        "verify: cargo mismatch",
                                        { "station" => name, "actual" => format!("{:?}", resp.inventory), "expected" => format!("{:?}", expected_inv) }
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!("verify query failed for {}: {}", name, e);
                                all_match = false;
                            }
                        }
                    }
                    for name in &self.ship_names {
                        let actor_ref: ShipRef<_> = client.actor_ref(name.clone());
                        match actor_ref.query_ship(QueryShipRequest {}).await {
                            Ok(resp) => {
                                if self.model.is_ship_uncertain(name) {
                                    self.model.reconcile_ship(name, &resp);
                                } else {
                                    let expected_credits = self.model.ship_credits(name);
                                    assert_always!(
                                        resp.credits == expected_credits,
                                        "verify: ship credit mismatch",
                                        { "ship" => name, "actual" => resp.credits, "expected" => expected_credits }
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
                            }
                            Err(e) => {
                                tracing::warn!("verify ship query failed for {}: {}", name, e);
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

        // === End-of-run verification sweep (FDB quiescence pattern) ===
        Self::verification_sweep(
            client,
            &mut self.model,
            &self.station_names.clone(),
            &self.ship_names.clone(),
            ctx,
        )
        .await;

        // Publish final reconciled model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        // Shutdown: drop client
        drop(self.client.take());

        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // After end-of-run sweep, model should be reconciled.
        // Verify conservation if all entities are certain.
        if self.model.uncertain.is_empty() && self.model.uncertain_ships.is_empty() {
            let credit_sum = self.model.total_station_credits() + self.model.total_ship_credits();
            assert_always!(
                credit_sum == self.model.total_credits,
                "final credit conservation",
                { "sum" => credit_sum, "expected" => self.model.total_credits }
            );

            // Final cargo conservation
            for (commodity, &expected) in &self.model.total_cargo {
                let actual_stations = self.model.total_cargo_for(commodity);
                let actual_ships = self.model.total_ship_cargo_for(commodity);
                assert_always!(
                    actual_stations + actual_ships == expected,
                    "final cargo conservation",
                    { "commodity" => commodity, "actual" => actual_stations + actual_ships, "expected" => expected }
                );
            }
        }

        // Final non-negative check (per-entity, skip uncertain)
        for (name, station) in &self.model.stations {
            if !self.model.uncertain.contains(name) {
                assert_always!(station.credits >= 0, "final non-negative station credits", {
                    "station" => name, "credits" => station.credits
                });
            }
        }
        for (name, ship) in &self.model.ships {
            if !self.model.uncertain_ships.contains(name) {
                assert_always!(ship.credits >= 0, "final non-negative ship credits", {
                    "ship" => name, "credits" => ship.credits
                });
            }
        }

        // Publish final model
        ctx.state().publish(SPACE_MODEL_KEY, self.model.clone());

        Ok(())
    }
}
