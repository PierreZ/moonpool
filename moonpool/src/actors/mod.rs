//! Virtual actor networking layer for moonpool.
//!
//! Virtual actors sit on top of the existing transport layer — the transport
//! moves bytes, the actor layer adds identity, directory lookup, and method
//! dispatch.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────┐
//! │               Virtual Actor Layer                 │
//! │  ActorRouter → Directory → PlacementStrategy      │
//! │  ActorMessage carries identity + method + body    │
//! ├───────────────────────────────────────────────────┤
//! │               Transport Layer                     │
//! │  NetTransport → EndpointMap → Peer connections    │
//! │  Token 0 reserved for actor dispatch per type     │
//! └───────────────────────────────────────────────────┘
//! ```
//!
//! # Token Layout
//!
//! Static interfaces use one token per method (indices 1, 2, 3, …).
//! Virtual actors use ONE token (index 0) per actor type. All messages
//! for all instances of that actor type land in a single handler. The
//! handler dispatches by actor identity and method.
//!
//! ```text
//! EndpointMap:
//!   UID(0xCA1C, 1) → Calculator::add RequestStream     (static)
//!   UID(0xCA1C, 2) → Calculator::sub RequestStream     (static)
//!   UID(0x504C, 0) → PlayerActor handler               (virtual, all methods)
//! ```
//!
//! # Orleans Model
//!
//! Turn-based concurrency: one message at a time per actor instance.
//! The processing loop dequeues a message, finds/activates the actor,
//! calls the method, sends the response, then processes the next message.

mod directory;
mod placement;
mod router;
mod types;

pub use directory::{ActorDirectory, DirectoryError, InMemoryDirectory};
pub use placement::{LocalPlacement, PlacementError, PlacementStrategy};
pub use router::{ActorError, ActorRouter};
pub use types::{ActorId, ActorMessage, ActorResponse, ActorType};
