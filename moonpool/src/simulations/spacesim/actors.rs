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

    async fn query_state<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        _req: QueryStateRequest,
    ) -> Result<StationResponse, RpcError> {
        Ok(self.response())
    }
}

#[actor_impl(Station)]
impl ActorHandler for StationActorImpl {
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
