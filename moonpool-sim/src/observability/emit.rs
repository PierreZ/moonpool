//! Event emission helpers and the [`sim_emit!`](crate::sim_emit) macro.
//!
//! The macro captures a typed payload, stashes it in a thread-local, and emits
//! a `tracing::event!` at target `"moonpool::sim"`. The active
//! [`crate::observability::SimulationLayer`] retrieves the typed payload by
//! taking from the thread-local at `on_event` time.
//!
//! When no [`crate::observability::SimulationLayer`] is installed, the macro
//! still emits a tracing event so production subscribers (fmt, OTel) see the
//! event with its `Debug` representation. The thread-local stash and the layer
//! short-circuit are guarded by an [`AtomicI32`] install counter.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use super::event::AnyPayload;

/// One pending typed emission, handed from the macro to the layer.
pub(crate) struct PendingEmit {
    pub(crate) key: &'static str,
    pub(crate) time_ms: u64,
    pub(crate) source: String,
    pub(crate) payload: Arc<dyn AnyPayload>,
}

thread_local! {
    static PENDING: RefCell<Option<PendingEmit>> = const { RefCell::new(None) };
}

static INSTALL_COUNT: AtomicI32 = AtomicI32::new(0);

/// Returns true when at least one [`crate::observability::SimulationLayer`] is
/// currently installed as a default subscriber.
///
/// Used by the [`sim_emit!`](crate::sim_emit) macro to decide whether to allocate
/// the typed payload.
#[inline]
pub fn layer_installed() -> bool {
    INSTALL_COUNT.load(Ordering::Relaxed) > 0
}

pub(crate) fn increment_install_count() {
    INSTALL_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn decrement_install_count() {
    INSTALL_COUNT.fetch_sub(1, Ordering::Relaxed);
}

/// Stash a typed payload for the upcoming `tracing::event!`.
///
/// Called by the [`sim_emit!`](crate::sim_emit) macro before firing the event.
/// The layer's `on_event` retrieves the payload via [`take_pending`].
pub fn stash_pending(
    key: &'static str,
    time_ms: u64,
    source: impl Into<String>,
    payload: Arc<dyn AnyPayload>,
) {
    PENDING.with(|cell| {
        *cell.borrow_mut() = Some(PendingEmit {
            key,
            time_ms,
            source: source.into(),
            payload,
        });
    });
}

/// Take the currently pending emit, if any. Called by the layer's `on_event`.
pub(crate) fn take_pending() -> Option<PendingEmit> {
    PENDING.with(|cell| cell.borrow_mut().take())
}

/// Drop any pending stash (used by the macro on the reentrancy panic path).
pub fn clear_pending() {
    PENDING.with(|cell| {
        let _ = cell.borrow_mut().take();
    });
}

/// True iff a payload is still stashed in the thread-local. Used by the
/// [`sim_emit!`](crate::sim_emit) macro to detect when `tracing`'s dispatch
/// reentrancy guard suppressed our event (e.g. emitted from inside an
/// invariant's `observe` callback).
#[inline]
pub fn has_pending() -> bool {
    PENDING.with(|cell| cell.borrow().is_some())
}

/// Emit a typed simulation event to the active [`crate::observability::SimulationLayer`].
///
/// The first argument is a `&'static str` timeline key (e.g. [`crate::SIM_FAULT_TIMELINE`]).
/// The second is a typed payload.
///
/// `time_ms` and `source` must be supplied — the macro reads them from the
/// caller. For workload code, [`crate::SimContext::emit`] does this automatically.
///
/// In production (no layer installed), the macro still emits a `tracing::event!`
/// so logs see the event with `Debug` formatting; the typed payload is not
/// allocated.
#[macro_export]
macro_rules! sim_emit {
    ($key:expr, $time_ms:expr, $source:expr, $payload:expr $(,)?) => {{
        let __key: &'static str = $key;
        let __time_ms: u64 = $time_ms;
        let __source: ::std::string::String = ::std::convert::Into::into($source);
        if $crate::observability::emit::layer_installed() {
            let __payload = ::std::sync::Arc::new($payload);
            $crate::observability::emit::stash_pending(
                __key,
                __time_ms,
                __source.clone(),
                __payload.clone() as ::std::sync::Arc<dyn $crate::observability::event::AnyPayload>,
            );
            ::tracing::event!(
                target: "moonpool::sim",
                ::tracing::Level::INFO,
                key = __key,
                time_ms = __time_ms,
                source = ::tracing::field::display(&__source),
                payload = ::tracing::field::debug(&*__payload),
            );
            // If `tracing`'s dispatch reentrancy guard suppressed our event,
            // the layer never ran on_event and our payload is still stashed.
            // The likely cause is emission from inside an invariant's observe
            // callback — forbidden because it could deadlock the layer.
            if $crate::observability::emit::has_pending() {
                $crate::observability::emit::clear_pending();
                panic!(
                    "Invariant emitted a simulation event (forbidden — invariants must be read-only)"
                );
            }
        } else {
            ::tracing::event!(
                target: "moonpool::sim",
                ::tracing::Level::INFO,
                key = __key,
                time_ms = __time_ms,
                source = ::tracing::field::display(&__source),
                payload = ::tracing::field::debug(&$payload),
            );
        }
    }};
}
