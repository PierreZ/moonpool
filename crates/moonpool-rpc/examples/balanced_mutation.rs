//! What a balanced *mutation* may do under each permission, on real TCP.
//!
//! Two replicas of a bank account apply deposits to a shared journal (the
//! application's replicated state; RPC knows nothing about it). Replica A
//! applies a deposit and then loses the reply (its promise breaks): the
//! caller cannot tell whether the deposit happened. Replica B is healthy
//! but slow.
//!
//! 1. **Default (at most once).** The call fails with
//!    `Execution::MaybeExecuted` and is *not* sent to B: the deposit
//!    happened once, and the caller must find out (read the balance)
//!    before trying again.
//! 2. **Retry permission, blind deposit.** The call succeeds on B, and the
//!    deposit is applied **twice**. Retry permission is a promise by the
//!    application that repeating the request is harmless; a blind
//!    deposit is not.
//! 3. **Retry permission, idempotent deposit.** The request carries a
//!    deposit id and the journal applies each id once: the retry is
//!    harmless and the balance is right.
//! 4. **Hedge permission.** With A slow instead of broken, a hedge sends
//!    the same deposit to B while A is still working: both execute.
//!    Concurrent copies need the same idempotence as retries, and are a
//!    separate permission: at-most-once never sends one.
//!
//! Run with `cargo run -p moonpool-rpc --example balanced_mutation`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_core::{Providers, TimeProvider, TokioProviders};
use moonpool_rpc::balance::{
    Alternative, AlternativeSet, BalanceConfig, BalancePolicy, BalancedClient, HedgeBudget,
    Locality, ModelConfig, QueueModel, SetVersion,
};
use moonpool_rpc::{
    AccessClass, IncomingRequest, MethodId, RpcConfig, RpcDriver, RpcHandle, RpcMethod,
    SchemaVersion, ServiceRef,
};

/// Add `amount` to the account.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Deposit {
    /// Identifies the deposit when `idempotent` (0: blind).
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// How much.
    #[prost(uint64, tag = "2")]
    pub amount: u64,
    /// Apply each id at most once.
    #[prost(bool, tag = "3")]
    pub idempotent: bool,
}

/// The balance after the deposit.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Balance {
    /// The account balance.
    #[prost(uint64, tag = "1")]
    pub balance: u64,
}

/// The deposit method.
pub struct Deposits;
impl RpcMethod for Deposits {
    type Request = Deposit;
    type Reply = Balance;
    const METHOD: MethodId = MethodId::new(0xDE90);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "deposit";
}

/// The replicated account both replicas apply to.
#[derive(Default)]
struct Journal {
    balance: u64,
    applied: BTreeSet<u64>,
}

impl Journal {
    fn apply(&mut self, deposit: &Deposit) -> u64 {
        if !deposit.idempotent || self.applied.insert(deposit.id) {
            self.balance += deposit.amount;
        }
        self.balance
    }
}

/// How a replica treats deposits.
#[derive(Clone, Copy)]
enum Replica {
    /// Apply, then break the promise.
    LosesReplies,
    /// Apply, then answer after a delay.
    Slow(Duration),
}

async fn replica(
    kind: Replica,
    journal: Arc<Mutex<Journal>>,
) -> Result<ServiceRef<Deposits>, Box<dyn std::error::Error>> {
    let providers = TokioProviders::new();
    let (driver, rpc) =
        RpcDriver::listen(providers.clone(), "127.0.0.1:0", RpcConfig::default()).await?;
    tokio::spawn(driver.run());
    let (service, mut stream) = rpc.register::<Deposits>(AccessClass::Public)?;
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let balance = journal
                .lock()
                .expect("Mutex poisoned: prior task panicked")
                .apply(&request);
            let time = providers.time().clone();
            tokio::spawn(async move {
                match kind {
                    Replica::LosesReplies => drop(reply),
                    Replica::Slow(delay) => {
                        let _ = time.sleep(delay).await;
                        let _ = reply.send(&Balance { balance });
                    }
                }
            });
        }
    });
    Ok(service)
}

/// A client preferring `first` (same machine) over `second`.
fn client(
    rpc: &RpcHandle<TokioProviders>,
    first: &ServiceRef<Deposits>,
    second: &ServiceRef<Deposits>,
) -> Result<BalancedClient<TokioProviders, Deposits>, Box<dyn std::error::Error>> {
    let set = AlternativeSet::new(
        SetVersion::new(1),
        vec![
            Alternative::new(first.clone(), Locality::new("m1", "dc1")),
            Alternative::new(second.clone(), Locality::in_datacenter("dc2")),
        ],
    )?;
    let model = QueueModel::new(ModelConfig {
        hedge_budget: HedgeBudget {
            initial: 10.0,
            ..HedgeBudget::default()
        },
        ..ModelConfig::default()
    })?;
    tokio::spawn(model.collect_lagging());
    Ok(BalancedClient::new(
        rpc,
        set,
        model,
        BalanceConfig {
            locality: Locality::new("m1", "dc1"),
            ..BalanceConfig::default()
        },
    )?)
}

fn balance(journal: &Mutex<Journal>) -> u64 {
    journal
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .balance
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), RpcConfig::default())?;
    tokio::spawn(driver.run());
    let journal = Arc::new(Mutex::new(Journal::default()));
    let broken = replica(Replica::LosesReplies, Arc::clone(&journal)).await?;
    let healthy = replica(
        Replica::Slow(Duration::from_millis(5)),
        Arc::clone(&journal),
    )
    .await?;
    let balanced = client(&rpc, &broken, &healthy)?;
    let blind = Deposit {
        id: 0,
        amount: 10,
        idempotent: false,
    };

    // 1. At most once: ambiguous, not retried.
    let error = balanced
        .call(blind.clone(), &BalancePolicy::at_most_once())
        .await
        .expect_err("the reply is lost");
    println!(
        "1. at most once: {error}\n   balance {} (the deposit happened once; the caller cannot know)",
        balance(&journal)
    );
    assert_eq!(balance(&journal), 10);

    // 2. Retry permission on a blind deposit: applied twice.
    let done = balanced
        .call(blind, &BalancePolicy::idempotent())
        .await
        .expect("B answers");
    println!(
        "2. retry, blind deposit: ok after {} attempts, balance {} (+20 for one deposit of 10)",
        done.attempts.len(),
        done.reply.balance
    );
    assert_eq!(balance(&journal), 30);

    // 3. Retry permission on an idempotent deposit: applied once.
    let keyed = Deposit {
        id: 7,
        amount: 10,
        idempotent: true,
    };
    let done = balanced
        .call(keyed, &BalancePolicy::idempotent())
        .await
        .expect("B answers");
    println!(
        "3. retry, deposit id 7: ok after {} attempts, balance {} (+10)",
        done.attempts.len(),
        done.reply.balance
    );
    assert_eq!(balance(&journal), 40);

    // 4. Hedging: A is slow now, B fast; both run the blind deposit.
    let slow = replica(
        Replica::Slow(Duration::from_millis(200)),
        Arc::clone(&journal),
    )
    .await?;
    let fast = replica(Replica::Slow(Duration::ZERO), Arc::clone(&journal)).await?;
    let hedging = client(&rpc, &slow, &fast)?;
    let blind = Deposit {
        id: 0,
        amount: 10,
        idempotent: false,
    };
    let single = hedging
        .call(blind.clone(), &BalancePolicy::at_most_once())
        .await
        .expect("A answers, slowly");
    println!(
        "4. at most once, slow replica: {} attempt, balance {} (+10)",
        single.attempts.len(),
        balance(&journal)
    );
    let hedged = hedging
        .call(blind, &BalancePolicy::hedged())
        .await
        .expect("the hedge answers");
    TokioProviders::new()
        .time()
        .sleep(Duration::from_millis(300))
        .await?;
    println!(
        "   hedged: {} attempts, balance {} (+20: both copies ran)",
        hedged.attempts.len(),
        balance(&journal)
    );
    assert_eq!(balance(&journal), 70);
    Ok(())
}
