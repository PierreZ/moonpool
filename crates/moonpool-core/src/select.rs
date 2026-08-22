//! Deterministic `select!`: tokio's own expansion, driven by a seeded offset.
//!
//! This file is short on code and long on explanation, on purpose: the
//! mechanism fits in one macro rule, but it leans on how `tokio::select!`
//! is built internally. Read this doc top to bottom once and the macro at
//! the bottom will look obvious.
//!
//! ## The problem
//!
//! `select!` waits on several futures at once and runs the handler of the
//! first one that completes. When two branches become ready at the same
//! moment, SOMETHING has to decide which handler runs. In tokio that
//! decision is the polling order: every time the select future is polled,
//! tokio picks a start index and polls the branches in the order
//! `start, start+1, ... (mod N)`. The first branch found ready wins.
//!
//! The start index is random so that no branch can starve the others. That
//! randomness comes from tokio's thread-local RNG (`thread_rng_n`), which
//! is:
//!
//! - seedable only through the unstable `tokio_unstable` runtime API, and
//! - seeded from process entropy when there is no tokio runtime at all,
//!   which is exactly the moonpool situation: there is NO tokio runtime
//!   inside a simulation.
//!
//! For a simulation that must replay a seed bit-for-bit this is a
//! correctness hole: the same seed would pick different branch winners on
//! every run of the test binary.
//!
//! ## How tokio's macro is built (the part we reuse)
//!
//! `tokio::select!` is a `macro_rules!` macro organized as a three-stage
//! pipeline (see tokio's `src/macros/select.rs`):
//!
//! ```text
//! stage 1: entry rules      recognize the overall shape of the call
//! stage 2: normalize rules  munch the branches one by one (tagged with @)
//! stage 3: transform rule   emit the real code: the polling loop
//! ```
//!
//! The line that matters is tokio's own fair-mode ENTRY rule (abridged):
//!
//! ```text
//! ($p:pat = $($t:tt)*) => {
//!     $crate::select!(@{ start={ thread_rng_n(BRANCHES) }; () } $p = $($t)*)
//! };
//! ```
//!
//! Notice what it does NOT do: it does not call the RNG. Macros only move
//! tokens around; the RNG call is written, unevaluated, into a slot named
//! `start`, and everything is forwarded to stage 2. Stages 2 and 3 carry
//! the slot along untouched (they match it as `start=$start:expr`), and
//! stage 3 finally pastes it INSIDE the polling closure it emits:
//!
//! ```text
//! poll_fn(|cx| {
//!     // ...
//!     let start = $start;               // evaluated HERE, on EVERY poll
//!     for i in 0..BRANCHES {
//!         let branch = (start + i) % BRANCHES;
//!         // ... poll that branch, first Ready wins ...
//!     }
//! })
//! ```
//!
//! So the randomness source is not baked into the machinery at all. It is
//! a plug-in expression chosen by whoever writes the entry rule. tokio's
//! entry rule plugs in `thread_rng_n(BRANCHES)`. Nothing stops a different
//! entry rule from plugging in something else, because `macro_rules!` has
//! no private rules: the stage-2 and stage-3 rules live in the same
//! `#[macro_export]`ed `select!` macro, so `tokio::select!(@{ ... } ...)`
//! is callable from any crate.
//!
//! ## What moonpool's `select!` does
//!
//! The macro below IS that different entry rule. For the default (fair)
//! form it jumps straight into tokio's stage 2, plugging moonpool's seeded
//! source into the `start` slot:
//!
//! ```text
//! tokio::select!(@{ start={ select_support::select_offset(BRANCHES) }; () } ...)
//! ```
//!
//! Everything after the entry rule is tokio's machinery, byte for byte:
//! branch polling, the disabled-branch mask, `if` preconditions, refutable
//! patterns, `else`, cooperative budgeting, the 64-branch cap, source-order
//! evaluation of the future expressions, and the per-poll redraw of the
//! offset. We copied none of it and we maintain none of it. The ONLY thing
//! that changes is where the number comes from.
//!
//! ## Why the `BRANCHES` token resolves (macro hygiene)
//!
//! `BRANCHES` is a `const` holding the branch count, defined by stage 3
//! inside the block it emits. Our entry rule writes the token `BRANCHES`
//! into the `start` expression BEFORE that const exists anywhere. Two
//! macro-hygiene facts make this sound:
//!
//! - `macro_rules!` hygiene only isolates local variables (`let` bindings
//!   and labels). Items are not isolated, and a `const` is an item: any
//!   code that ends up inside its scope can name it, no matter which macro
//!   wrote which token.
//! - tokio itself depends on exactly this. Its own entry rule writes the
//!   `BRANCHES` token in one macro expansion, and the const is defined by
//!   a later, separate expansion (stage 3). Our token rides through the
//!   identical path, so it resolves the same way tokio's own token does.
//!
//! ## Where the offset comes from at runtime
//!
//! [`select_support::select_offset`](crate::select_support) is the number
//! source:
//!
//! - **Simulation**: moonpool-sim installs a seeded stream per iteration
//!   ([`set_select_offset_override`](crate::select_support::set_select_offset_override)),
//!   so the same seed replays the same winners exactly and different seeds
//!   explore different orderings.
//! - **Production** (no override installed): a thread-local entropy-seeded
//!   RNG, behaviorally what tokio's fair mode does.
//!
//! ## The internal-format dependency (the price)
//!
//! The `@{ start=...; () }` entry shape is tokio's `#[doc(hidden)]` macro
//! plumbing, not covered by semver. Two facts make that acceptable:
//!
//! - The shape has been stable since tokio 1.4 (2021, when `biased;`
//!   landed) and is identical in every release since.
//! - The failure mode is loud. If a future tokio changes the shape, every
//!   fair-mode `select!` call site stops compiling with a macro-match error
//!   on the upgrade commit. It can not drift silently; winner-selection and
//!   determinism tests pin the semantics on top.
//!
//! If it ever breaks, the fallbacks are to vendor tokio's `select.rs` or to
//! resurrect the rotation combinator this macro replaced (git history).
//!
//! ## The build-vs-test asymmetry
//!
//! Which macro you get is decided by cargo feature unification, not by the
//! code you compile. Without `deterministic-select` this crate re-exports
//! tokio's `select!` verbatim. The moment moonpool-sim enters the build
//! graph, even as a dev-dependency, this macro replaces it for EVERY crate
//! in that graph, including your library code compiled for tests. Both forms
//! are tokio's own expansion with identical grammar and limits, so the swap
//! changes nothing but the offset source.

/// Waits on multiple concurrent branches, returning when the **first**
/// completes, with a deterministic, seed-controlled polling order.
///
/// Drop-in replacement for [`tokio::select!`] with the same grammar:
///
/// ```text
/// select! {
///     <pattern> = <async expression> (, if <precondition>)? => <handler>,
///     ...
///     (else => <handler>)?
/// }
/// ```
///
/// By default the branch polled first is chosen, on every poll, by an offset
/// drawn from [`select_support::select_offset`](crate::select_support):
/// inside a moonpool simulation that source is seeded (same seed, same
/// choices, exact replay; different seeds explore different orderings), and
/// outside a simulation it is entropy-based, matching tokio's production
/// behavior.
///
/// `select! { biased; ... }` skips the draw entirely and forwards verbatim to
/// `tokio::select! { biased; ... }`: branches are polled top to bottom, which
/// is already fully deterministic. Use it when branches have a natural
/// priority (shutdown or timeout guards); use the default form when branches
/// are peers racing for the same wake (message queues, notify channels), so
/// the simulation can explore both winners across seeds.
///
/// # Examples
///
/// ```
/// # futures::executor::block_on(async {
/// let winner = moonpool_core::select! {
///     x = async { 1 } => x,
///     _ = std::future::pending::<()>() => unreachable!(),
/// };
/// assert_eq!(winner, 1);
/// # });
/// ```
///
/// See the [module docs](crate::select) for the full walkthrough of how the
/// offset is injected into tokio's own expansion.
#[macro_export]
macro_rules! select {
    // ------------------------------------------------------------------
    // Rule 1: `biased;` mode.
    //
    // The caller asked for source-order polling, which involves no
    // randomness anywhere (tokio hard-codes start=0 for it). There is
    // nothing to make deterministic, so forward the whole call verbatim
    // to the real tokio macro.
    // ------------------------------------------------------------------
    (biased; $($branches:tt)*) => {
        $crate::__tokio::select! { biased; $($branches)* }
    };

    // ------------------------------------------------------------------
    // Rule 2: `else`-only input, e.g. `select! { else => 42 }`.
    //
    // No branches means no polling order and no draw. Forward verbatim so
    // tokio applies its own rule (it evaluates the handler directly) or
    // emits its own diagnostic for malformed input.
    //
    // This rule must be tried before rule 3: `else` is a keyword, so it
    // can never start the `$p:pat` fragment below, but keeping it first
    // makes the priority explicit.
    // ------------------------------------------------------------------
    (else => $($rest:tt)*) => {
        $crate::__tokio::select! { else => $($rest)* }
    };

    // ------------------------------------------------------------------
    // Rule 3: the default (fair) mode. THE interesting rule.
    //
    // `$p:pat = $($t:tt)*` matches the same shape as tokio's own fair-mode
    // entry rule: the first branch's pattern, then everything else as raw
    // tokens. Instead of letting tokio's entry rule run (it would plug its
    // entropy RNG into the `start` slot), we invoke tokio's stage-2 rules
    // directly, with moonpool's offset source in the slot:
    //
    // - `@{ ... }` is tokio's internal marker for "input already passed
    //   the entry rule, start normalizing branches".
    // - `start={ ... }` is the polling-order expression. It travels through
    //   the pipeline as unevaluated tokens and is pasted inside the
    //   `poll_fn` closure that tokio's stage 3 emits, so it is re-evaluated
    //   on EVERY poll of the select (exactly like tokio's fair mode).
    // - `BRANCHES` is the branch-count const that stage 3 defines in the
    //   scope where this expression lands. Items are unhygienic across
    //   macro_rules expansions, and tokio's own entry rule relies on the
    //   very same cross-expansion resolution (see the module docs).
    // - `()` is stage 2's branch counter, empty at the start.
    //
    // `select_offset` receives the branch count and returns the index to
    // poll first (already reduced modulo the count).
    // ------------------------------------------------------------------
    ($p:pat = $($t:tt)*) => {
        $crate::__tokio::select!(
            @{ start={ $crate::select_support::select_offset(BRANCHES) }; () } $p = $($t)*
        )
    };

    // ------------------------------------------------------------------
    // Rule 4: empty input. Forward so tokio emits its own diagnostic
    // ("select! requires at least one branch").
    // ------------------------------------------------------------------
    () => {
        $crate::__tokio::select! {}
    };
}
