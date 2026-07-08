//! Deterministic `select!`: a seeded rotation combinator over `tokio::select!`.
//!
//! ## Why this exists
//!
//! `tokio::select!` randomizes which branch it polls first so that no branch
//! starves. That random start offset comes from tokio's thread-local RNG,
//! which is only seedable through the unstable `tokio_unstable` runtime API;
//! outside a tokio runtime it silently falls back to OS entropy. For a
//! deterministic simulation that must replay a seed bit-for-bit, that is a
//! correctness hole: the same seed picks different branch winners on every
//! process run.
//!
//! ## How it works
//!
//! tokio's "fair" mode is exactly its `biased;` mode started at a random
//! index: it polls branch `(start + i) % N`. Polling the branch list rotated
//! left by `start`, in order, is the same schedule. So this macro draws
//! `start` from a moonpool-controlled source and dispatches over the `N`
//! rotations of a *biased* tokio select:
//!
//! ```text
//! match select_offset(3) {
//!     0 => tokio::select! { biased; A, B, C },
//!     1 => tokio::select! { biased; B, C, A },
//!     _ => tokio::select! { biased; C, A, B },
//! }
//! ```
//!
//! Every polling mechanic (branch disabling, `if` preconditions, refutable
//! patterns, `else`, cooperative budgeting) is inherited from the real
//! `tokio::select!` expansion; nothing is reimplemented here.
//!
//! ## Divergences from `tokio::select!`
//!
//! - Future *expressions* are evaluated in rotated order, not source order.
//!   This only matters if constructing a future has side effects whose order
//!   is observable.
//! - The offset is drawn once per `select!` *execution*; tokio redraws on
//!   every poll of the same execution. Each new execution (e.g. each loop
//!   iteration) draws fresh.
//! - At most 16 branches (the expansion emits one rotated copy per branch);
//!   tokio allows 64.
//! - Handler code is duplicated once per rotation, and no two copies of an
//!   anonymous type (async block, closure) are the same type. If a handler
//!   feeds such a value into a container whose element type is inferred
//!   (e.g. `FuturesUnordered`), name the type instead: `.boxed()` /
//!   `.boxed_local()` the future, or construct it through a shared `fn`.
//!
//! ## The build-vs-test asymmetry
//!
//! Which macro you get is decided by cargo feature unification, not by the
//! code you compile. Without `deterministic-select` this crate re-exports
//! tokio's `select!` verbatim (64 branches, no duplication). The moment
//! moonpool-sim enters the build graph, even as a dev-dependency, the
//! combinator replaces it for EVERY crate in that graph, including your
//! library code compiled for tests. Code that only compiles under the
//! passthrough (17+ branches, or handlers relying on anonymous-type
//! identity) therefore passes `cargo build` and fails `cargo test`. Write
//! selects within the combinator's limits everywhere to stay portable
//! across both worlds.

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
/// By default the branch polled first is chosen by a rotation offset drawn
/// from [`select_support::select_offset`](crate::select_support): inside a
/// moonpool simulation that source is seeded (same seed, same choices, exact
/// replay; different seeds explore different orderings), and outside a
/// simulation it is entropy-based, matching tokio's production behavior.
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
/// See the [module docs](crate::select) for the rotation strategy and the
/// documented divergences from `tokio::select!`.
#[macro_export]
macro_rules! select {
    // `biased;`: source-order priority, no randomness anywhere. Forward verbatim.
    (biased; $($branches:tt)*) => {
        $crate::__tokio::select! { biased; $($branches)* }
    };
    // Empty input: forward so tokio emits its own diagnostic.
    () => {
        $crate::__tokio::select! {}
    };
    // Fair mode: split the branches, then dispatch over seeded rotations.
    ($($input:tt)*) => {
        $crate::__select_split! { acc = [] ; $($input)* }
    };
}

/// Splits `select!` input into branch groups plus an optional trailing
/// `else`, then hands off to `__select_rotate!`.
///
/// Each branch is stored as a single `{ ... }` token group holding its source
/// tokens (`pat = future`, optional `, if cond`, `=> handler`). Expression
/// handlers are normalized into blocks (`=> h` becomes `=> { h }`): a
/// captured `:expr` fragment cannot re-match tokio's `:block` matchers, so
/// every handler must reach `tokio::select!` as a block.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_split {
    // -- terminal states ----------------------------------------------------
    // No branches at all (else-only input): tokio also rejects this.
    (acc = [] ; else => $($rest:tt)*) => {
        compile_error!("select! must contain at least one branch besides `else`")
    };
    // Input exhausted: rotate with no else branch. The branch groups double
    // as a unary counter (`remaining`): each group is one token tree.
    (acc = [$($branch:tt)+] ; ) => {
        $crate::__select_rotate! {
            index = []
            remaining = [$($branch)+]
            order = [$($branch)+]
            arms = []
            els = []
        }
    };
    // Trailing `else` (block or expression handler): must be last.
    (acc = [$($branch:tt)+] ; else => $handler:block $(,)?) => {
        $crate::__select_rotate! {
            index = []
            remaining = [$($branch)+]
            order = [$($branch)+]
            arms = []
            els = [$handler]
        }
    };
    (acc = [$($branch:tt)+] ; else => $handler:expr $(,)?) => {
        $crate::__select_rotate! {
            index = []
            remaining = [$($branch)+]
            order = [$($branch)+]
            arms = []
            els = [{ $handler }]
        }
    };

    // -- branch munchers (comma variants before comma-less ones) ------------
    // Precondition + block handler.
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr, if $c:expr => $h:block , $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f, if $c => $h }] ; $($rest)* }
    };
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr, if $c:expr => $h:block $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f, if $c => $h }] ; $($rest)* }
    };
    // Precondition + expression handler (comma required unless last).
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr, if $c:expr => $h:expr , $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f, if $c => { $h } }] ; $($rest)* }
    };
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr, if $c:expr => $h:expr) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f, if $c => { $h } }] ; }
    };
    // No precondition + block handler.
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr => $h:block , $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f => $h }] ; $($rest)* }
    };
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr => $h:block $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f => $h }] ; $($rest)* }
    };
    // No precondition + expression handler (comma required unless last).
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr => $h:expr , $($rest:tt)*) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f => { $h } }] ; $($rest)* }
    };
    (acc = [$($acc:tt)*] ; $p:pat = $f:expr => $h:expr) => {
        $crate::__select_split! { acc = [$($acc)* { $p = $f => { $h } }] ; }
    };
}

/// Emits the `match select_offset(N) { ... }` dispatch, one arm per rotation.
///
/// Token-counter recursion: `index` (unary `_`s) numbers the arm being
/// emitted, `remaining` (the branch groups themselves, one token tree each)
/// counts down by one per step, and `order` is rotated left by one branch
/// between arms. The final rotation becomes the `_` arm so the match is
/// exhaustive.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_rotate {
    // More than one rotation left: emit a literal-index arm, rotate, recurse.
    (
        index = [$($i:tt)*]
        remaining = [$rhead:tt $($rrest:tt)+]
        order = [$first:tt $($rest:tt)*]
        arms = [$($arms:tt)*]
        els = [$($els:tt)*]
    ) => {
        $crate::__select_rotate! {
            index = [$($i)* _]
            remaining = [$($rrest)+]
            order = [$($rest)* $first]
            arms = [
                $($arms)*
                $crate::__select_count!($($i)*) =>
                    $crate::__select_emit! { branches = [$first $($rest)*] els = [$($els)*] },
            ]
            els = [$($els)*]
        }
    };
    // Last rotation: close with the wildcard arm and emit the dispatch.
    (
        index = [$($i:tt)*]
        remaining = [$rlast:tt]
        order = [$($order:tt)*]
        arms = [$($arms:tt)*]
        els = [$($els:tt)*]
    ) => {
        // `order` always holds all N branch groups (left rotation preserves
        // count), so it doubles as the branch-count source for the draw.
        match $crate::select_support::select_offset($crate::__select_count!($($order)*)) {
            $($arms)*
            _ => $crate::__select_emit! { branches = [$($order)*] els = [$($els)*] },
        }
    };
}

/// Emits one rotation as a real `tokio::select! { biased; ... }`.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_emit {
    (branches = [$( { $($branch:tt)* } )+] els = []) => {
        $crate::__tokio::select! { biased; $( $($branch)* , )+ }
    };
    (branches = [$( { $($branch:tt)* } )+] els = [$handler:tt]) => {
        $crate::__tokio::select! { biased; $( $($branch)* , )+ else => $handler }
    };
}

/// Counts token trees (0..=16) into a `u32` literal, usable in both
/// expression and match-pattern position. The inputs are either unary `_`
/// counters (`index`) or whole branch groups (`total`), one token tree each.
/// The fallback arm enforces the 16-branch cap.
#[doc(hidden)]
#[macro_export]
macro_rules! __select_count {
    () => {
        0u32
    };
    ($a:tt) => {
        1u32
    };
    ($a:tt $b:tt) => {
        2u32
    };
    ($a:tt $b:tt $c:tt) => {
        3u32
    };
    ($a:tt $b:tt $c:tt $d:tt) => {
        4u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt) => {
        5u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt) => {
        6u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt) => {
        7u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt) => {
        8u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt) => {
        9u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt) => {
        10u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt) => {
        11u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt $l:tt) => {
        12u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt $l:tt $m:tt) => {
        13u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt $l:tt $m:tt $n:tt) => {
        14u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt $l:tt $m:tt $n:tt $o:tt) => {
        15u32
    };
    ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt $i:tt $j:tt $k:tt $l:tt $m:tt $n:tt $o:tt $p:tt) => {
        16u32
    };
    ($($too_many:tt)*) => {
        compile_error!("moonpool select! supports at most 16 branches")
    };
}
