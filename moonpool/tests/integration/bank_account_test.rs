//! Integration test for BankAccountActor end-to-end message flow.

use moonpool::prelude::*;
use std::rc::Rc;

// Import the BankAccountActor and related types
mod bank_account {
    pub use crate::simulation::bank_account::actor::{
        BankAccountActor, DepositRequest, GetBalanceRequest, WithdrawRequest,
        dispatch_bank_account_message,
    };
}

#[tokio::test]
async fn test_bank_account_end_to_end() {
    // Initialize tracing
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    // Run test in LocalSet for spawn_local support
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            run_test().await;
        })
        .await;
}

async fn run_test() {
    tracing::info!("Starting bank account end-to-end test");

    // Step 1: Create infrastructure
    let node_id = NodeId::from("127.0.0.1:5000").expect("Failed to create NodeId");
    let message_bus = Rc::new(moonpool::messaging::MessageBus::new(node_id.clone()));

    // Step 2: Create actor and catalog
    let actor_id =
        ActorId::from_string("test::BankAccount/alice").expect("Failed to create ActorId");
    let actor = bank_account::BankAccountActor::new(actor_id.clone());

    let catalog = Rc::new(ActorCatalog::<bank_account::BankAccountActor>::new(node_id));
    catalog.set_message_bus(message_bus.clone());
    message_bus.set_actor_router(catalog.clone());

    // Step 3: Create activation and activate
    let context = catalog
        .get_or_create_activation(actor_id.clone(), actor)
        .expect("Failed to create activation");

    context
        .activate(None)
        .await
        .expect("Failed to activate actor");

    context.set_message_bus(message_bus.clone());

    // Step 4: Get ActorRef
    let actor_ref = ActorRef::<bank_account::BankAccountActor>::with_message_bus(
        actor_id.clone(),
        message_bus.clone(),
    );

    // Step 5: Perform operations
    tracing::info!("Depositing 100");
    actor_ref
        .send(bank_account::DepositRequest { amount: 100 })
        .await
        .expect("Failed to send deposit");
    context
        .process_message_queue(bank_account::dispatch_bank_account_message)
        .await
        .expect("Failed to process deposit");

    tracing::info!("Depositing 50");
    actor_ref
        .send(bank_account::DepositRequest { amount: 50 })
        .await
        .expect("Failed to send second deposit");
    context
        .process_message_queue(bank_account::dispatch_bank_account_message)
        .await
        .expect("Failed to process second deposit");

    tracing::info!("Withdrawing 30");
    actor_ref
        .send(bank_account::WithdrawRequest { amount: 30 })
        .await
        .expect("Failed to send withdrawal");
    context
        .process_message_queue(bank_account::dispatch_bank_account_message)
        .await
        .expect("Failed to process withdrawal");

    // Step 6: Check balance
    tracing::info!("Checking balance");
    // Spawn the call as a task since it will block waiting for response
    let actor_ref_clone = actor_ref.clone();
    let call_task = tokio::task::spawn_local(async move {
        actor_ref_clone
            .call::<bank_account::GetBalanceRequest, u64>(bank_account::GetBalanceRequest)
            .await
    });

    // Give the call time to send the request
    tokio::task::yield_now().await;

    // Process the request message
    context
        .process_message_queue(bank_account::dispatch_bank_account_message)
        .await
        .expect("Failed to process balance request");

    // Now await the response
    let balance = call_task
        .await
        .expect("Task panicked")
        .expect("Failed to get balance");

    // Step 7: Verify
    tracing::info!("Final balance: {}", balance);
    assert_eq!(balance, 120, "Expected balance of 120, got {}", balance);

    tracing::info!("Bank account end-to-end test completed successfully!");
}
