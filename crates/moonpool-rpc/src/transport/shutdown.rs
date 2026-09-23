//! Graceful shutdown: close admission, drain within a deadline on provider
//! time, end what remains with honest execution knowledge, close every
//! connection.

use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moonpool_core::{Providers, TimeProvider};

use super::connection::CloseReason;
use super::{Command, DRAINING, PendingCall, RUNNING, Shared, TERMINATED};
use crate::error::{ErrorReason, Execution, RpcError};
use crate::stats::Counters;

/// How often a draining runtime checks whether it drained, on provider
/// time.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// What a graceful shutdown ([`RpcHandle::shutdown`](crate::RpcHandle::shutdown))
/// did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShutdownReport {
    /// The runtime was already gone (its driver dropped) or another
    /// shutdown had already finished: nothing was done.
    pub already_stopped: bool,
    /// Everything admitted and pending finished before the deadline.
    pub drained: bool,
    /// Calls and reply streams this runtime was still waiting on at the
    /// deadline, failed with [`ErrorReason::Shutdown`] and the execution
    /// knowledge they had ([`Execution::NotAdmitted`] if never sent,
    /// [`Execution::MaybeExecuted`] otherwise).
    pub calls_ended: usize,
    /// Replies and streams still owed to callers at the deadline: their
    /// sessions closed under them, so each caller sees a disconnect after
    /// admission (`MaybeExecuted`), never a fabricated outcome.
    pub replies_abandoned: usize,
    /// Connections open at the deadline and closed by the shutdown.
    pub connections_closed: usize,
    /// Every connection's driver finished (streams closed, a TLS session's
    /// `close_notify` sent) within the close bound, and the listener is
    /// closed.
    pub closed_cleanly: bool,
}

impl<P: Providers> Shared<P> {
    /// Stop admitting and starting work. Returns `false` if a shutdown
    /// already began.
    pub(super) fn begin_shutdown(&self) -> bool {
        let began = self
            .lifecycle
            .compare_exchange(RUNNING, DRAINING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if began {
            tracing::info!(address = ?self.address, "rpc_shutdown_started");
            // Wakes the driver, which closes the listener.
            let _ = self.commands.unbounded_send(Command::StopAccepting);
        }
        began
    }

    /// Nothing is pending, owed, produced or queued any more.
    fn drained(&self) -> bool {
        let pending = self.lock().pending.is_empty();
        pending
            && self.counters.inflight_requests.load(Ordering::Relaxed) == 0
            && self.counters.live_server_streams.load(Ordering::Relaxed) == 0
            && self.counters.queued_bytes.load(Ordering::Relaxed) == 0
    }

    /// End everything left: fail every pending call honestly and close
    /// every connection.
    fn terminate(&self, report: &mut ShutdownReport) {
        self.lifecycle.store(TERMINATED, Ordering::Release);
        report.replies_abandoned = self.counters.inflight_requests.load(Ordering::Relaxed)
            + self.counters.live_server_streams.load(Ordering::Relaxed);
        let mut state = self.lock();
        let pending: Vec<(u64, PendingCall)> =
            std::mem::take(&mut state.pending).into_iter().collect();
        let mut ended = Vec::with_capacity(pending.len());
        for (call_id, mut call) in pending {
            // A request still queued behind others is withdrawn, so "never
            // sent" stays provable.
            if let super::Origin::Connection(id) = call.origin
                && !call.transmitted
                && let Some(connection) = state.connections.get(&id)
            {
                call.transmitted = !connection.retract(call_id);
            }
            let execution = if call.may_have_executed() {
                Execution::MaybeExecuted
            } else {
                Execution::NotAdmitted
            };
            ended.push((call, RpcError::new(ErrorReason::Shutdown, execution)));
        }
        let connections: Vec<_> = state.connections.values().cloned().collect();
        // Every endpoint closes: its queued, never-received requests are
        // dropped (their callers' sessions close below) and its receivers
        // see the end of their stream.
        let mut registrations = state.registry.drain();
        registrations.extend(std::mem::take(&mut state.well_known).into_values());
        drop(state);
        report.calls_ended = ended.len();
        report.connections_closed = connections.len();
        for (call, error) in ended {
            Counters::bump(&self.counters.calls_ended_by_shutdown);
            call.completion.finish(Err(error));
        }
        for connection in connections {
            // Each driver then reports the end (nothing pending is left to
            // fail) and ends the streams it produced.
            let _ = connection.close(CloseReason::Local);
        }
        // After the sessions closed, so nothing (a broken promise) is
        // written for the dropped requests.
        drop(registrations);
        self.watch.notify();
    }

    fn lock_report(&self) -> std::sync::MutexGuard<'_, Option<ShutdownReport>> {
        self.shutdown_report
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// Whether every driver child (listener, connections) ended.
    fn quiet(&self) -> bool {
        self.counters.live_tasks.load(Ordering::Relaxed) == 0 && self.lock().connections.is_empty()
    }
}

/// The graceful shutdown of the runtime behind `weak`.
pub(super) async fn shutdown<P: Providers>(
    weak: Weak<Shared<P>>,
    grace: Duration,
) -> ShutdownReport {
    let mut report = ShutdownReport::default();
    let Some(shared) = weak.upgrade() else {
        report.already_stopped = true;
        return report;
    };
    let time = shared.time().clone();
    if !shared.begin_shutdown() {
        // Another shutdown runs (or ran): its deadline governs; report its
        // outcome.
        drop(shared);
        loop {
            let Some(shared) = weak.upgrade() else {
                report.already_stopped = true;
                return report;
            };
            if let Some(outcome) = *shared.lock_report() {
                return outcome;
            }
            drop(shared);
            let _ = time.sleep(DRAIN_POLL).await;
        }
    }
    let close_bound = shared.config.handshake_timeout;
    // Never hold the runtime across a wait: dropping the driver must still
    // drop it (an abrupt shutdown overtakes a graceful one).
    drop(shared);
    let start = time.now();
    loop {
        let Some(shared) = weak.upgrade() else {
            report.already_stopped = true;
            return report;
        };
        if shared.drained() {
            report.drained = true;
            break;
        }
        let elapsed = time.now().saturating_sub(start);
        if elapsed >= grace {
            break;
        }
        drop(shared);
        let _ = time
            .sleep(DRAIN_POLL.min(grace.saturating_sub(elapsed)))
            .await;
    }
    if let Some(shared) = weak.upgrade() {
        shared.terminate(&mut report);
        tracing::info!(
            drained = report.drained,
            calls_ended = report.calls_ended,
            replies_abandoned = report.replies_abandoned,
            connections_closed = report.connections_closed,
            "rpc_shutdown_drained"
        );
    }
    let closing = time.now();
    loop {
        let Some(shared) = weak.upgrade() else {
            // Dropped meanwhile: everything went with it.
            report.closed_cleanly = false;
            return report;
        };
        if shared.quiet() {
            report.closed_cleanly = true;
            *shared.lock_report() = Some(report);
            return report;
        }
        let elapsed = time.now().saturating_sub(closing);
        if elapsed >= close_bound {
            *shared.lock_report() = Some(report);
            return report;
        }
        drop(shared);
        let _ = time
            .sleep(DRAIN_POLL.min(close_bound.saturating_sub(elapsed)))
            .await;
    }
}
