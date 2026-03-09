//! StationActor: virtual actor for space station economy.
//!
//! Defines the `Station` service trait and `StationActorImpl` handler
//! with persistent state, `DeactivateOnIdle` lifecycle, and credit/inventory
//! management operations.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::actors::{ActorContext, ActorError, ActorHandler, DeactivationHint, PersistentState};
use crate::{MessageCodec, Providers, RpcError, actor_impl, service};

/// Request to deposit credits into a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositCreditsRequest {
    /// Amount to deposit.
    pub amount: i64,
}

/// Request to withdraw credits from a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawCreditsRequest {
    /// Amount to withdraw.
    pub amount: i64,
}

/// Request to add cargo to a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddCargoRequest {
    /// Commodity name.
    pub commodity: String,
    /// Amount to add.
    pub amount: i64,
}

/// Request to remove cargo from a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveCargoRequest {
    /// Commodity name.
    pub commodity: String,
    /// Amount to remove.
    pub amount: i64,
}

/// Request to execute a trade at the station (atomic cargo + credit transfer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteTradeRequest {
    /// Commodity to trade.
    pub commodity: String,
    /// Amount of commodity.
    pub amount: i64,
    /// Price in credits.
    pub price: i64,
    /// Direction from the ship's perspective.
    pub direction: TradeDirection,
}

/// Request to query a station's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryStateRequest {}

/// Response containing current station state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StationResponse {
    /// Current credit balance.
    pub credits: i64,
    /// Current inventory.
    pub inventory: BTreeMap<String, i64>,
}

/// Persistent state for a station actor.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct StationData {
    /// Credit balance.
    pub credits: i64,
    /// Inventory of goods.
    pub inventory: BTreeMap<String, i64>,
}

/// Define the station virtual actor interface.
#[service(id = 0x5741_7100)]
pub trait Station {
    /// Deposit credits and return updated state.
    async fn deposit_credits(
        &mut self,
        req: DepositCreditsRequest,
    ) -> Result<StationResponse, RpcError>;

    /// Withdraw credits and return updated state.
    async fn withdraw_credits(
        &mut self,
        req: WithdrawCreditsRequest,
    ) -> Result<StationResponse, RpcError>;

    /// Add cargo to the station.
    async fn add_cargo(&mut self, req: AddCargoRequest) -> Result<StationResponse, RpcError>;

    /// Remove cargo from the station.
    async fn remove_cargo(&mut self, req: RemoveCargoRequest) -> Result<StationResponse, RpcError>;

    /// Execute a trade atomically: adjust both cargo and credits in one operation.
    async fn execute_trade(
        &mut self,
        req: ExecuteTradeRequest,
    ) -> Result<StationResponse, RpcError>;

    /// Query the current station state.
    async fn query_state(&mut self, req: QueryStateRequest) -> Result<StationResponse, RpcError>;
}

/// Station actor implementation with persistent state.
///
/// Uses `DeactivateOnIdle` for maximum lifecycle exercise during simulation.
#[derive(Default)]
pub struct StationActorImpl {
    state: Option<PersistentState<StationData>>,
}

impl StationActorImpl {
    fn data(&self) -> StationData {
        self.state
            .as_ref()
            .map(|s| s.state().clone())
            .unwrap_or_default()
    }

    fn response(&self) -> StationResponse {
        let data = self.data();
        StationResponse {
            credits: data.credits,
            inventory: data.inventory,
        }
    }
}

#[async_trait(?Send)]
impl Station for StationActorImpl {
    async fn deposit_credits<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: DepositCreditsRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            s.state_mut().credits += req.amount;
            let _ = s.write_state().await;
        }
        Ok(self.response())
    }

    async fn withdraw_credits<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: WithdrawCreditsRequest,
    ) -> Result<StationResponse, RpcError> {
        // Balance checking is done at the workload level (like banking sim).
        // The actor always succeeds.
        if let Some(s) = &mut self.state {
            s.state_mut().credits -= req.amount;
            let _ = s.write_state().await;
        }
        Ok(self.response())
    }

    async fn add_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: AddCargoRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            *s.state_mut().inventory.entry(req.commodity).or_insert(0) += req.amount;
            let _ = s.write_state().await;
        }
        Ok(self.response())
    }

    async fn remove_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: RemoveCargoRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            *s.state_mut().inventory.entry(req.commodity).or_insert(0) -= req.amount;
            let _ = s.write_state().await;
        }
        Ok(self.response())
    }

    async fn execute_trade<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: ExecuteTradeRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            match req.direction {
                TradeDirection::Buy => {
                    // Ship buys from station: station loses cargo, gains credits
                    *s.state_mut().inventory.entry(req.commodity).or_insert(0) -= req.amount;
                    s.state_mut().credits += req.price;
                }
                TradeDirection::Sell => {
                    // Ship sells to station: station gains cargo, loses credits
                    *s.state_mut().inventory.entry(req.commodity).or_insert(0) += req.amount;
                    s.state_mut().credits -= req.price;
                }
            }
            let _ = s.write_state().await;
        }
        Ok(self.response())
    }

    async fn query_state<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        _req: QueryStateRequest,
    ) -> Result<StationResponse, RpcError> {
        Ok(self.response())
    }
}

// ============================================================================
// ShipActor: virtual actor for trading ships
// ============================================================================

/// Direction of a trade between a ship and a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeDirection {
    /// Ship buys cargo from station (pays credits, receives cargo).
    Buy,
    /// Ship sells cargo to station (gives cargo, receives credits).
    Sell,
}

/// Request to execute a trade between a ship and a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRequest {
    /// Target station to trade with.
    pub station: String,
    /// Commodity to trade.
    pub commodity: String,
    /// Amount of commodity to trade.
    pub amount: i64,
    /// Price in credits for the trade.
    pub price: i64,
    /// Direction of the trade.
    pub direction: TradeDirection,
}

/// Request to query a ship's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryShipRequest {}

/// Response containing current ship state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShipResponse {
    /// Current credit balance.
    pub credits: i64,
    /// Current cargo manifest.
    pub cargo: BTreeMap<String, i64>,
    /// Whether the trade was executed (false if precondition failed).
    pub traded: bool,
}

/// Persistent state for a ship actor.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ShipData {
    /// Credit balance.
    pub credits: i64,
    /// Cargo manifest.
    pub cargo: BTreeMap<String, i64>,
}

/// Define the ship virtual actor interface.
#[service(id = 0x5348_1900)]
pub trait Ship {
    /// Execute a trade with a station.
    async fn trade(&mut self, req: TradeRequest) -> Result<ShipResponse, RpcError>;

    /// Query the current ship state.
    async fn query_ship(&mut self, req: QueryShipRequest) -> Result<ShipResponse, RpcError>;
}

/// Ship actor implementation with persistent state and cross-actor trading.
#[derive(Default)]
pub struct ShipActorImpl {
    state: Option<PersistentState<ShipData>>,
}

impl ShipActorImpl {
    fn data(&self) -> ShipData {
        self.state
            .as_ref()
            .map(|s| s.state().clone())
            .unwrap_or_default()
    }

    fn response(&self, traded: bool) -> ShipResponse {
        let data = self.data();
        ShipResponse {
            credits: data.credits,
            cargo: data.cargo,
            traded,
        }
    }
}

#[async_trait(?Send)]
impl Ship for ShipActorImpl {
    async fn trade<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        req: TradeRequest,
    ) -> Result<ShipResponse, RpcError> {
        // Pre-check ship's own state
        match req.direction {
            TradeDirection::Buy => {
                if self.data().credits < req.price {
                    return Ok(self.response(false));
                }
            }
            TradeDirection::Sell => {
                if self.data().cargo.get(&req.commodity).copied().unwrap_or(0) < req.amount {
                    return Ok(self.response(false));
                }
            }
        }

        // Single atomic station RPC — both cargo and credit transfer in one call
        let station_ref: StationRef<P, C> = ctx.actor_ref(req.station.clone());
        station_ref
            .execute_trade(ExecuteTradeRequest {
                commodity: req.commodity.clone(),
                amount: req.amount,
                price: req.price,
                direction: req.direction.clone(),
            })
            .await?;

        // Station call succeeded — update ship state locally
        if let Some(s) = &mut self.state {
            match req.direction {
                TradeDirection::Buy => {
                    s.state_mut().credits -= req.price;
                    *s.state_mut().cargo.entry(req.commodity).or_insert(0) += req.amount;
                }
                TradeDirection::Sell => {
                    *s.state_mut().cargo.entry(req.commodity).or_insert(0) -= req.amount;
                    s.state_mut().credits += req.price;
                }
            }
            let _ = s.write_state().await;
        }
        Ok(self.response(true))
    }

    async fn query_ship<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        _req: QueryShipRequest,
    ) -> Result<ShipResponse, RpcError> {
        Ok(self.response(false))
    }
}

#[actor_impl(Ship)]
impl ActorHandler for ShipActorImpl {
    fn placement_strategy() -> crate::actors::PlacementStrategy {
        crate::actors::PlacementStrategy::RoundRobin
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps = PersistentState::<ShipData>::load(store.clone(), "Ship", &ctx.id.identity)
                .await
                .map_err(|e| ActorError::HandlerError(format!("state load: {e}")))?;
            self.state = Some(ps);
        }
        Ok(())
    }
}

#[actor_impl(Station)]
impl ActorHandler for StationActorImpl {
    fn placement_strategy() -> crate::actors::PlacementStrategy {
        crate::actors::PlacementStrategy::RoundRobin
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps =
                PersistentState::<StationData>::load(store.clone(), "Station", &ctx.id.identity)
                    .await
                    .map_err(|e| ActorError::HandlerError(format!("state load: {e}")))?;
            self.state = Some(ps);
        }
        Ok(())
    }
}
