//! Operation alphabet for virtual actor simulation testing.
//!
//! Defines all possible operations that can be performed on the banking
//! actor system, enabling randomized exploration of the state space.

use moonpool::RandomProvider;

/// Operations that can be performed on the banking actor system.
///
/// Categorized into Normal (everyday operations), Adversarial (edge cases),
/// and Nemesis (fault injection) operations.
#[derive(Debug, Clone)]
pub enum ActorOp {
    // ---- Normal (60%) ----
    /// Deposit funds into an actor's account.
    Deposit { actor_id: String, amount: u64 },

    /// Withdraw funds from an actor's account.
    Withdraw { actor_id: String, amount: u64 },

    /// Query the balance of an actor's account.
    GetBalance { actor_id: String },

    /// Transfer funds between two actors.
    Transfer {
        from: String,
        to: String,
        amount: u64,
    },

    // ---- Adversarial (25%) ----
    /// Send a request to an actor that hasn't been created yet.
    SendToNonExistent { actor_id: String },

    /// Send an invalid method discriminant.
    InvalidMethod { actor_id: String, method: u32 },

    /// Send multiple concurrent calls to the same actor.
    ConcurrentCallsSameActor { actor_id: String, count: u32 },

    /// Deposit zero amount (edge case).
    ZeroAmountDeposit { actor_id: String },

    /// Withdraw the maximum possible amount.
    MaxAmountWithdraw { actor_id: String },

    // ---- Nemesis (15%) ----
    /// Trigger actor deactivation via idle timeout.
    DeactivateActor { actor_id: String },

    /// Flood a single actor with many rapid requests.
    FloodSingleActor { actor_id: String, count: u32 },
}

/// Weights controlling the probability distribution of operation types.
#[derive(Debug, Clone)]
pub struct OpWeights {
    // Normal
    pub deposit: u32,
    pub withdraw: u32,
    pub get_balance: u32,
    pub transfer: u32,
    // Adversarial
    pub send_to_non_existent: u32,
    pub invalid_method: u32,
    pub concurrent_calls: u32,
    pub zero_amount_deposit: u32,
    pub max_amount_withdraw: u32,
    // Nemesis
    pub deactivate_actor: u32,
    pub flood_single_actor: u32,
}

impl Default for OpWeights {
    fn default() -> Self {
        Self {
            // Normal (60%)
            deposit: 20,
            withdraw: 15,
            get_balance: 15,
            transfer: 10,
            // Adversarial (25%)
            send_to_non_existent: 5,
            invalid_method: 5,
            concurrent_calls: 5,
            zero_amount_deposit: 5,
            max_amount_withdraw: 5,
            // Nemesis (15%)
            deactivate_actor: 8,
            flood_single_actor: 7,
        }
    }
}

impl OpWeights {
    /// Normal operations only (for happy-path testing).
    pub fn normal_only() -> Self {
        Self {
            deposit: 30,
            withdraw: 25,
            get_balance: 25,
            transfer: 20,
            send_to_non_existent: 0,
            invalid_method: 0,
            concurrent_calls: 0,
            zero_amount_deposit: 0,
            max_amount_withdraw: 0,
            deactivate_actor: 0,
            flood_single_actor: 0,
        }
    }

    /// Normal + adversarial operations.
    pub fn with_adversarial() -> Self {
        Self {
            deposit: 20,
            withdraw: 15,
            get_balance: 10,
            transfer: 10,
            send_to_non_existent: 8,
            invalid_method: 8,
            concurrent_calls: 8,
            zero_amount_deposit: 8,
            max_amount_withdraw: 8,
            deactivate_actor: 0,
            flood_single_actor: 0,
        }
    }

    fn total_weight(&self) -> u32 {
        self.deposit
            + self.withdraw
            + self.get_balance
            + self.transfer
            + self.send_to_non_existent
            + self.invalid_method
            + self.concurrent_calls
            + self.zero_amount_deposit
            + self.max_amount_withdraw
            + self.deactivate_actor
            + self.flood_single_actor
    }
}

/// Generate a random operation from the alphabet.
///
/// Uses the provided random source and weight distribution to select
/// an operation. Actor IDs are picked from a pool of `actor_count` actors.
pub fn generate_operation<R: RandomProvider>(
    random: &R,
    weights: &OpWeights,
    actor_count: usize,
) -> ActorOp {
    let total = weights.total_weight();
    let choice = random.random_range(0..total);

    let actor_id = || format!("actor-{}", random.random_range(0..actor_count as u32));
    let different_actor = |from: &str| {
        if actor_count <= 1 {
            return from.to_string();
        }
        loop {
            let id = format!("actor-{}", random.random_range(0..actor_count as u32));
            if id != from {
                return id;
            }
        }
    };

    let mut cumulative = 0u32;

    // Normal operations
    cumulative += weights.deposit;
    if choice < cumulative {
        let amount = random.random_range(1..500u64);
        return ActorOp::Deposit {
            actor_id: actor_id(),
            amount,
        };
    }

    cumulative += weights.withdraw;
    if choice < cumulative {
        let amount = random.random_range(1..200u64);
        return ActorOp::Withdraw {
            actor_id: actor_id(),
            amount,
        };
    }

    cumulative += weights.get_balance;
    if choice < cumulative {
        return ActorOp::GetBalance {
            actor_id: actor_id(),
        };
    }

    cumulative += weights.transfer;
    if choice < cumulative {
        let from = actor_id();
        let to = different_actor(&from);
        let amount = random.random_range(1..100u64);
        return ActorOp::Transfer { from, to, amount };
    }

    // Adversarial operations
    cumulative += weights.send_to_non_existent;
    if choice < cumulative {
        return ActorOp::SendToNonExistent {
            actor_id: format!("nonexistent-{}", random.random_range(1000..9999u32)),
        };
    }

    cumulative += weights.invalid_method;
    if choice < cumulative {
        let method = random.random_range(50..100u32);
        return ActorOp::InvalidMethod {
            actor_id: actor_id(),
            method,
        };
    }

    cumulative += weights.concurrent_calls;
    if choice < cumulative {
        let count = random.random_range(2..5u32);
        return ActorOp::ConcurrentCallsSameActor {
            actor_id: actor_id(),
            count,
        };
    }

    cumulative += weights.zero_amount_deposit;
    if choice < cumulative {
        return ActorOp::ZeroAmountDeposit {
            actor_id: actor_id(),
        };
    }

    cumulative += weights.max_amount_withdraw;
    if choice < cumulative {
        return ActorOp::MaxAmountWithdraw {
            actor_id: actor_id(),
        };
    }

    // Nemesis operations
    cumulative += weights.deactivate_actor;
    if choice < cumulative {
        return ActorOp::DeactivateActor {
            actor_id: actor_id(),
        };
    }

    let count = random.random_range(5..15u32);
    ActorOp::FloodSingleActor {
        actor_id: actor_id(),
        count,
    }
}
