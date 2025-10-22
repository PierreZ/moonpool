# Typed ActorRef Pattern

## Problem

`ActorRef<A>` provides generic `call()` and `send()` methods, but you have to construct request structs manually:

```rust
// Verbose - need to construct request objects every time
let balance = alice.call(DepositRequest { amount: 100 }).await?;
let balance = alice.call(WithdrawRequest { amount: 50 }).await?;
```

## Solution: Extension Trait Pattern

Create an extension trait that provides typed methods for your actor:

```rust
/// Extension trait for BankAccountActor references
#[async_trait(?Send)]
pub trait BankAccountRef {
    async fn deposit(&self, amount: u64) -> Result<u64, ActorError>;
    async fn withdraw(&self, amount: u64) -> Result<u64, ActorError>;
    async fn get_balance(&self) -> Result<u64, ActorError>;
}

/// Implement the trait for ActorRef<BankAccountActor>
#[async_trait(?Send)]
impl BankAccountRef for ActorRef<BankAccountActor> {
    async fn deposit(&self, amount: u64) -> Result<u64, ActorError> {
        self.call(DepositRequest { amount }).await
    }

    async fn withdraw(&self, amount: u64) -> Result<u64, ActorError> {
        self.call(WithdrawRequest { amount }).await
    }

    async fn get_balance(&self) -> Result<u64, ActorError> {
        self.call(GetBalanceRequest).await
    }
}
```

## Usage

Now you can call actor methods directly:

```rust
use moonpool::prelude::*;

// Get actor reference
let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;

// Use typed methods - much cleaner!
let balance = alice.deposit(100).await?;
let balance = alice.withdraw(50).await?;
let balance = alice.get_balance().await?;
```

## Copy-Paste Template

Here's a template you can copy-paste and customize:

```rust
// 1. Define your actor and message types
pub struct MyActor { /*...*/ }

#[derive(Serialize, Deserialize)]
pub struct MyRequest { /*...*/ }

// 2. Implement Actor trait
#[async_trait(?Send)]
impl Actor for MyActor {
    // ... lifecycle methods
}

// 3. Implement MessageHandler for each message type
#[async_trait(?Send)]
impl MessageHandler<MyRequest, MyResponse> for MyActor {
    async fn handle(&mut self, req: MyRequest, ctx: &ActorContext<Self>)
        -> Result<MyResponse, ActorError>
    {
        // ... handle logic
    }
}

// 4. Create extension trait (copy-paste this pattern!)
#[async_trait(?Send)]
pub trait MyActorRef {
    async fn my_method(&self, param: Type) -> Result<ReturnType, ActorError>;
}

#[async_trait(?Send)]
impl MyActorRef for ActorRef<MyActor> {
    async fn my_method(&self, param: Type) -> Result<ReturnType, ActorError> {
        self.call(MyRequest { param }).await
    }
}
```

## Benefits

1. **Type Safety**: Compile-time checking of method signatures
2. **IDE Support**: Autocomplete shows available methods
3. **Clean API**: Looks like regular async method calls
4. **Zero Cost**: Extension traits have no runtime overhead
5. **Flexible**: Add helper methods, validation, etc.

## Future: Proc Macro

We may add a proc macro in the future to generate this boilerplate automatically:

```rust
#[actor_ref_methods]
impl MyActor {
    async fn deposit(amount: u64) -> u64;
    async fn withdraw(amount: u64) -> u64;
}

// Generates the extension trait automatically!
```

But for now, the manual trait pattern works great and gives you full control.

## Comparison with Orleans

This pattern is inspired by Orleans "Grain References":

| Orleans (C#) | Moonpool (Rust) |
|--------------|-----------------|
| `IGrainProxy<T>` | `ActorRef<A>` |
| Interface methods | Extension trait methods |
| Code generation | Manual trait impl (or future proc macro) |
| `grain.Deposit(100)` | `actor.deposit(100).await?` |

The main difference is that Orleans uses C# code generation at compile time, while Rust uses extension traits (which are more flexible but require a bit of boilerplate).
