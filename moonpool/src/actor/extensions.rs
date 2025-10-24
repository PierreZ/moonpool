//! Extension trait pattern for typed ActorRef methods.
//!
//! This module demonstrates how to create typed, ergonomic methods on `ActorRef<A>`
//! that wrap the generic `call()` and `send()` methods.
//!
//! # Quick Start

#![allow(clippy::empty_line_after_doc_comments)]
//!
//! Copy this template and replace with your actor's methods:
//!
//! ```rust,ignore
//! use moonpool::prelude::*;
//!
//! // 1. Define your extension trait
//! #[async_trait(?Send)]
//! pub trait MyActorRef {
//!     async fn my_method(&self, param: Type) -> Result<ReturnType, ActorError>;
//! }
//!
//! // 2. Implement for ActorRef<YourActor>
//! #[async_trait(?Send)]
//! impl MyActorRef for ActorRef<MyActor> {
//!     async fn my_method(&self, param: Type) -> Result<ReturnType, ActorError> {
//!         self.call(MyRequest { param }).await
//!     }
//! }
//!
//! // 3. Use it!
//! let actor = runtime.get_actor::<MyActor>("MyActor", "key")?;
//! let result = actor.my_method(value).await?;
//! ```
//!
//! # Pattern Overview
//!
//! Instead of writing:
//! ```rust,ignore
//! actor.call(DepositRequest { amount: 100 }).await?
//! ```
//!
//! You can write:
//! ```rust,ignore
//! actor.deposit(100).await?
//! ```
//!
//! # Full Example
//!
//! See the `examples/bank_account_typed.rs` for a complete working example.

/// Template trait for creating typed ActorRef extensions.
///
/// # Copy-Paste Template
///
/// Replace `ExampleActor` with your actor type and add your methods:
///
/// ```rust,ignore
/// #[async_trait(?Send)]
/// pub trait MyActorRef {
///     // Add your typed methods here
///     async fn method_name(&self, param: Type) -> Result<ReturnType, ActorError>;
/// }
///
/// #[async_trait(?Send)]
/// impl MyActorRef for ActorRef<MyActor> {
///     async fn method_name(&self, param: Type) -> Result<ReturnType, ActorError> {
///         // Call the generic call() method with your request type
///         self.call(MyRequest { param }).await
///     }
/// }
/// ```
///
/// # One-Way Messages
///
/// For fire-and-forget messages, use `send()`:
///
/// ```rust,ignore
/// async fn log_event(&self, message: String) -> Result<(), ActorError> {
///     self.send(LogEvent { message }).await
/// }
/// ```
///
/// # Custom Timeouts
///
/// For operations that need custom timeouts:
///
/// ```rust,ignore
/// async fn slow_operation(&self) -> Result<Data, ActorError> {
///     self.call_with_timeout(
///         SlowRequest,
///         Duration::from_secs(60)
///     ).await
/// }
/// ```
///
/// # Builder Pattern
///
/// You can combine this with builder patterns for complex requests:
///
/// ```rust,ignore
/// async fn transfer(&self, to: &ActorId, amount: u64, memo: Option<String>)
///     -> Result<(), ActorError>
/// {
///     self.call(TransferRequest {
///         to: to.clone(),
///         amount,
///         memo,
///     }).await
/// }
/// ```
///
/// # Error Handling
///
/// Extension methods inherit the error handling from `call()`:
/// - `ActorError::Timeout` - Response not received
/// - `ActorError::NodeUnavailable` - Target unreachable
/// - `ActorError::ProcessingFailed` - Actor returned error
/// - `ActorError::Message` - Serialization error
///
/// # Benefits
///
/// 1. **Type Safety**: Compile-time checking of parameters
/// 2. **IDE Support**: Autocomplete and documentation
/// 3. **Clean API**: Looks like regular async methods
/// 4. **Zero Cost**: No runtime overhead
/// 5. **Flexible**: Add validation, logging, etc.
///
/// # Comparison with Direct call()
///
/// | Direct call() | Extension Trait |
/// |---------------|-----------------|
/// | `actor.call(DepositRequest { amount }).await?` | `actor.deposit(amount).await?` |
/// | Generic, verbose | Typed, concise |
/// | Manual request construction | Automatic |
/// | No IDE hints for actor methods | Full autocomplete |
// No public trait here - this module is purely documentation.
// See the tests below for a working example of the pattern.

#[cfg(test)]
mod tests {
    use crate::actor::{Actor, ActorId, ActorRef, DeactivationReason};
    use crate::error::ActorError;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    // Example actor for testing the pattern
    struct ExampleActor {
        actor_id: ActorId,
    }

    #[async_trait(?Send)]
    impl Actor for ExampleActor {
        type State = ();
        const ACTOR_TYPE: &'static str = "ExampleActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(
            &mut self,
            _state: crate::actor::ActorState<Self::State>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    // Example request type
    #[derive(Debug, Serialize, Deserialize)]
    #[allow(dead_code)]
    struct ExampleRequest {
        value: u64,
    }

    // Example extension trait
    #[async_trait(?Send)]
    #[allow(dead_code)]
    trait ExampleActorRef {
        async fn example_method(&self, value: u64) -> Result<u64, ActorError>;
    }

    // Example implementation
    #[async_trait(?Send)]
    impl ExampleActorRef for ActorRef<ExampleActor> {
        async fn example_method(&self, value: u64) -> Result<u64, ActorError> {
            self.call(ExampleRequest { value }).await
        }
    }

    #[test]
    fn test_extension_trait_compiles() {
        // This test just verifies the pattern compiles correctly
        let actor_id = ActorId::from_string("test::Example/1").unwrap();
        let _actor_ref = ActorRef::<ExampleActor>::new(actor_id);

        // Can't actually call the method since it's not implemented,
        // but we can verify the trait is properly defined
    }
}
