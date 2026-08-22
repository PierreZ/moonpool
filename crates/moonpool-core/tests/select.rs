//! Behavior tests for the deterministic `select!` macro (tokio's expansion
//! with a moonpool-controlled start offset).
//!
//! These run WITHOUT any async runtime (`futures::executor::block_on`), which
//! is itself part of the proof: the macro must work on a foreign executor,
//! where tokio's own fair `select!` would fall back to OS entropy.
#![cfg(feature = "deterministic-select")]

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::executor::block_on;
use moonpool_core::select_support::set_select_offset_override;

/// Three always-ready branches returning their own index: with start
/// offset `k`, branch `k` is polled first and wins.
fn three_way() -> u32 {
    block_on(async {
        moonpool_core::select! {
            a = async { 0_u32 } => a,
            b = async { 1_u32 } => b,
            c = async { 2_u32 } => c,
        }
    })
}

static THREE_WAY_OFFSET: AtomicU32 = AtomicU32::new(0);

fn three_way_offset(_branches: u32) -> u32 {
    THREE_WAY_OFFSET.load(Ordering::Relaxed)
}

#[test]
fn start_offset_picks_the_winner() {
    set_select_offset_override(Some(three_way_offset));
    for k in 0..3 {
        THREE_WAY_OFFSET.store(k, Ordering::Relaxed);
        assert_eq!(three_way(), k, "offset {k} must make branch {k} win");
    }
    set_select_offset_override(None);
}

#[test]
fn biased_mode_polls_in_source_order() {
    // No override installed: biased mode must not draw an offset at all.
    let winner = block_on(async {
        moonpool_core::select! {
            biased;
            a = async { 1_u32 } => a,
            b = async { 2_u32 } => b,
        }
    });
    assert_eq!(winner, 1);
}

fn offset_zero(_branches: u32) -> u32 {
    0
}

#[test]
fn precondition_disables_a_branch() {
    set_select_offset_override(Some(offset_zero));
    let winner = block_on(async {
        moonpool_core::select! {
            a = async { 1_u32 }, if false => a,
            b = async { 2_u32 } => b,
        }
    });
    assert_eq!(
        winner, 2,
        "disabled branch must be skipped even when polled first"
    );
    set_select_offset_override(None);
}

#[test]
fn refutable_pattern_falls_through() {
    set_select_offset_override(Some(offset_zero));
    let winner = block_on(async {
        moonpool_core::select! {
            Some(v) = async { None::<u32> } => v,
            w = async { 7_u32 } => w,
        }
    });
    assert_eq!(winner, 7);
    set_select_offset_override(None);
}

static ELSE_OFFSET: AtomicU32 = AtomicU32::new(0);

fn else_offset(_branches: u32) -> u32 {
    ELSE_OFFSET.load(Ordering::Relaxed)
}

#[test]
fn else_fires_when_all_branches_are_disabled() {
    // The else handler must fire whatever the drawn offset is.
    set_select_offset_override(Some(else_offset));
    for k in 0..2 {
        ELSE_OFFSET.store(k, Ordering::Relaxed);
        let winner = block_on(async {
            moonpool_core::select! {
                a = async { 1_u32 }, if false => a,
                b = async { 2_u32 }, if false => b,
                else => 42_u32,
            }
        });
        assert_eq!(winner, 42, "offset {k}");
    }
    set_select_offset_override(None);
}

#[test]
fn single_branch_without_trailing_comma() {
    let winner = block_on(async {
        moonpool_core::select! { x = async { 5_u32 } => x }
    });
    assert_eq!(winner, 5);
}

#[test]
fn borrowed_future_branch() {
    // Mirrors the delivery.rs idiom: polling a pinned future by `&mut` reference.
    let winner = block_on(async {
        let mut signal = std::future::pending::<()>();
        moonpool_core::select! {
            r = async { 1_u32 } => r,
            () = &mut signal => 0_u32,
        }
    });
    assert_eq!(winner, 1);
}

#[test]
fn loop_control_flow_in_handlers() {
    let mut hits = 0_u32;
    block_on(async {
        loop {
            moonpool_core::select! {
                () = async {} => {
                    hits += 1;
                    if hits == 3 {
                        break;
                    }
                },
                () = std::future::pending::<()>() => {},
            }
        }
    });
    assert_eq!(hits, 3);
}

thread_local! {
    static SEQ: Cell<u32> = const { Cell::new(0) };
}

fn sequenced_offset(branches: u32) -> u32 {
    SEQ.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    }) % branches
}

#[test]
fn identical_offset_sequences_give_identical_winners() {
    set_select_offset_override(Some(sequenced_offset));
    let run = || -> Vec<u32> {
        SEQ.with(|c| c.set(0));
        (0..10).map(|_| three_way()).collect()
    };
    let first = run();
    let second = run();
    assert_eq!(first, second, "same offset stream must replay identically");
    assert!(
        first.iter().any(|&w| w != first[0]),
        "a varying offset stream must produce varying winners"
    );
    set_select_offset_override(None);
}

static SIXTEEN_OFFSET: AtomicU32 = AtomicU32::new(0);

fn sixteen_offset(_branches: u32) -> u32 {
    SIXTEEN_OFFSET.load(Ordering::Relaxed)
}

#[test]
fn sixteen_branches_at_every_offset() {
    set_select_offset_override(Some(sixteen_offset));
    for k in 0..16 {
        SIXTEEN_OFFSET.store(k, Ordering::Relaxed);
        let winner = block_on(async {
            moonpool_core::select! {
                v = async { 0_u32 } => v,
                v = async { 1_u32 } => v,
                v = async { 2_u32 } => v,
                v = async { 3_u32 } => v,
                v = async { 4_u32 } => v,
                v = async { 5_u32 } => v,
                v = async { 6_u32 } => v,
                v = async { 7_u32 } => v,
                v = async { 8_u32 } => v,
                v = async { 9_u32 } => v,
                v = async { 10_u32 } => v,
                v = async { 11_u32 } => v,
                v = async { 12_u32 } => v,
                v = async { 13_u32 } => v,
                v = async { 14_u32 } => v,
                v = async { 15_u32 } => v,
            }
        });
        assert_eq!(winner, k, "offset {k} must make branch {k} win");
    }
    set_select_offset_override(None);
}

static TWENTY_OFFSET: AtomicU32 = AtomicU32::new(0);

fn twenty_offset(_branches: u32) -> u32 {
    TWENTY_OFFSET.load(Ordering::Relaxed)
}

#[test]
fn twenty_branches_beyond_the_former_cap() {
    // The rotation combinator this macro replaced capped out at 16 branches;
    // tokio's own expansion (which we now enter directly) allows 64.
    set_select_offset_override(Some(twenty_offset));
    for k in 0..20 {
        TWENTY_OFFSET.store(k, Ordering::Relaxed);
        let winner = block_on(async {
            moonpool_core::select! {
                v = async { 0_u32 } => v,
                v = async { 1_u32 } => v,
                v = async { 2_u32 } => v,
                v = async { 3_u32 } => v,
                v = async { 4_u32 } => v,
                v = async { 5_u32 } => v,
                v = async { 6_u32 } => v,
                v = async { 7_u32 } => v,
                v = async { 8_u32 } => v,
                v = async { 9_u32 } => v,
                v = async { 10_u32 } => v,
                v = async { 11_u32 } => v,
                v = async { 12_u32 } => v,
                v = async { 13_u32 } => v,
                v = async { 14_u32 } => v,
                v = async { 15_u32 } => v,
                v = async { 16_u32 } => v,
                v = async { 17_u32 } => v,
                v = async { 18_u32 } => v,
                v = async { 19_u32 } => v,
            }
        });
        assert_eq!(winner, k, "offset {k} must make branch {k} win");
    }
    set_select_offset_override(None);
}

/// Future that returns `Pending` (self-waking) a fixed number of times, then
/// `Ready`.
struct ReadyAfter {
    pending_polls: u32,
}

impl std::future::Future for ReadyAfter {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.pending_polls == 0 {
            std::task::Poll::Ready(())
        } else {
            self.pending_polls -= 1;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

static DRAW_COUNT: AtomicU32 = AtomicU32::new(0);

fn counting_offset(_branches: u32) -> u32 {
    DRAW_COUNT.fetch_add(1, Ordering::Relaxed);
    0
}

#[test]
fn offset_is_redrawn_on_every_poll() {
    // tokio's fair mode redraws the start offset on each poll of the same
    // select execution; the injected expression must inherit that.
    set_select_offset_override(Some(counting_offset));
    DRAW_COUNT.store(0, Ordering::Relaxed);
    block_on(async {
        moonpool_core::select! {
            () = ReadyAfter { pending_polls: 2 } => {},
            () = std::future::pending::<()>() => {},
        }
    });
    assert_eq!(
        DRAW_COUNT.load(Ordering::Relaxed),
        3,
        "three polls of one execution must draw three offsets"
    );
    set_select_offset_override(None);
}

#[test]
fn entropy_fallback_still_completes() {
    // No override: production behavior. One branch pending, so the winner is
    // fixed regardless of the entropy-drawn offset.
    set_select_offset_override(None);
    let winner = block_on(async {
        moonpool_core::select! {
            r = async { 9_u32 } => r,
            () = std::future::pending::<()>() => 0_u32,
        }
    });
    assert_eq!(winner, 9);
}
