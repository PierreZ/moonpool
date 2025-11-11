//! Test actors for simulation testing.

pub mod ping_pong_actor;

pub use ping_pong_actor::{
    PingPongActor, PingPongActorFactory, PingPongActorRef, PingRequest, PongResponse,
};
