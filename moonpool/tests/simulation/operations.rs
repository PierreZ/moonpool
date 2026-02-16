//! Operation alphabet for actor simulation workloads.
//!
//! Defines the set of operations that can be randomly generated and executed
//! against the banking actor system. Weights control the distribution.

use moonpool::RandomProvider;
use moonpool_sim::SimRandomProvider;

/// An operation in the banking actor alphabet.
pub enum ActorOp {
    /// Deposit funds into an account.
    Deposit { account: String, amount: i64 },
    /// Withdraw funds from an account.
    Withdraw { account: String, amount: i64 },
    /// Query the current balance of an account.
    GetBalance { account: String },
    /// Transfer funds between two accounts.
    Transfer {
        from: String,
        to: String,
        amount: i64,
    },
    /// Small delay to let background tasks run.
    SmallDelay,
}

/// Generate a random operation from the alphabet.
///
/// Weights: deposit 30%, withdraw 20%, get_balance 20%, transfer 20%, delay 10%.
pub fn random_op(random: &SimRandomProvider, accounts: &[String]) -> ActorOp {
    let roll = random.random_range(0..100);
    let account = || accounts[random.random_range(0..accounts.len())].clone();

    match roll {
        0..30 => ActorOp::Deposit {
            account: account(),
            amount: random.random_range(1..100),
        },
        30..50 => ActorOp::Withdraw {
            account: account(),
            amount: random.random_range(1..50),
        },
        50..70 => ActorOp::GetBalance { account: account() },
        70..90 => {
            let from = account();
            let mut to = account();
            // Ensure different accounts for transfer
            while to == from && accounts.len() > 1 {
                to = account();
            }
            ActorOp::Transfer {
                from,
                to,
                amount: random.random_range(1..30),
            }
        }
        _ => ActorOp::SmallDelay,
    }
}
