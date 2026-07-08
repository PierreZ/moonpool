//! With `select` but WITHOUT `deterministic-select`, `moonpool_core::select!`
//! must be tokio's macro verbatim (the production passthrough).
#![cfg(all(feature = "select", not(feature = "deterministic-select")))]

use futures::executor::block_on;

#[test]
fn passthrough_biased_polls_in_source_order() {
    let winner = block_on(async {
        moonpool_core::select! {
            biased;
            a = async { 1_u32 } => a,
            b = async { 2_u32 } => b,
        }
    });
    assert_eq!(winner, 1);
}

#[test]
fn passthrough_fair_mode_works_off_any_runtime() {
    // tokio's fair select off-runtime draws an entropy offset; with a single
    // ready branch the winner is fixed either way.
    let winner = block_on(async {
        moonpool_core::select! {
            r = async { 3_u32 } => r,
            () = std::future::pending::<()>() => 0_u32,
        }
    });
    assert_eq!(winner, 3);
}
