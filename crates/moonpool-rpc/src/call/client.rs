//! Explicitly bound clients of service references.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Weak;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::channel::oneshot;
use moonpool_core::{Providers, TimeProvider};

use crate::codec::{Wire, encode_to_vec};
use crate::error::{CallIdentity, ErrorReason, Execution, RpcError};
use crate::interface::ServiceRef;
use crate::protocol::RpcMethod;
use crate::transport::{Delivery, ReplyBytes, RpcHandle};

/// A [`ServiceRef`] bound to a runtime: the calling side.
///
/// Cloning is cheap. A client holds the runtime weakly: it never keeps a
/// dropped runtime serving, and calls on it then fail with
/// [`ErrorReason::Shutdown`].
pub struct ServiceClient<P: Providers, M: RpcMethod> {
    rpc: RpcHandle<P>,
    target: ServiceRef<M>,
}

impl<P: Providers, M: RpcMethod> Clone for ServiceClient<P, M> {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            target: self.target.clone(),
        }
    }
}

impl<P: Providers, M: RpcMethod> std::fmt::Debug for ServiceClient<P, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceClient")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl<P: Providers, M: RpcMethod> ServiceClient<P, M> {
    pub(crate) fn new(rpc: RpcHandle<P>, target: ServiceRef<M>) -> Self {
        Self { rpc, target }
    }

    /// The reference this client calls.
    #[must_use]
    pub fn target(&self) -> &ServiceRef<M> {
        &self.target
    }

    fn encode(&self, request: &M::Request) -> Result<(Vec<u8>, CallIdentity), RpcError> {
        // A reference decoded inside another message is checked here, before
        // anything leaves: a wrong-typed or malformed one is never sent.
        self.target
            .check()
            .map_err(|error| RpcError::not_admitted(ErrorReason::InvalidReference(error.0)))?;
        let body = encode_to_vec(request)
            .map_err(|error| RpcError::not_admitted(ErrorReason::Encode(error.0)))?;
        let identity = CallIdentity {
            method: M::METHOD,
            schema: M::SCHEMA,
            codec: <M::Request as Wire>::CODEC,
        };
        Ok((body, identity))
    }

    fn start(
        &self,
        request: &M::Request,
        delivery: Delivery,
    ) -> Result<ReplyAttempt<P, M>, RpcError> {
        let (body, identity) = self.encode(request)?;
        // Hold the runtime only while starting the call: the wait must not
        // keep a dropped driver's state alive.
        let shared = self
            .rpc
            .upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?;
        let (receiver, guard) =
            shared.start_call(&self.target.endpoint(), identity, body, delivery)?;
        Ok(ReplyAttempt {
            receiver,
            guard: Some(guard),
            time: shared.time().clone(),
            _method: PhantomData,
        })
    }

    /// Start one at-most-once attempt and return its handle.
    ///
    /// The attempt executes on the server zero or one times; nothing is
    /// retransmitted, on this connection or any later one. The handle is a
    /// future of the attempt's single completion. Keeping it after the
    /// caller has stopped caring (a hedged call whose sibling won) still
    /// yields the late outcome; dropping it releases the reply route, and a
    /// reply arriving later is counted
    /// ([`RpcStats::late_replies`](crate::RpcStats::late_replies)) and
    /// discarded. [`ReplyAttempt::cancel`] releases it and reports what is
    /// known about execution.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`] raised before the request was queued (always
    /// [`Execution::NotAdmitted`]).
    pub fn attempt(&self, request: &M::Request) -> Result<ReplyAttempt<P, M>, RpcError> {
        self.start(request, Delivery::AtMostOnce)
    }

    /// Send one request attempt and wait for its single completion.
    ///
    /// The request executes on the server zero or one times: nothing is
    /// retransmitted, on this connection or any later one. The outcome is
    /// the reply, or an [`RpcError`] stating what it proves about execution.
    ///
    /// Cancelling the returned future (dropping it, or a timeout around it)
    /// releases the reply route at once; a reply arriving later is counted
    /// and discarded. Cancellation does not retract a request already sent:
    /// the server may still execute it. Prefer
    /// [`try_get_reply_within`](Self::try_get_reply_within) for a deadline
    /// that reports what it knows.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`].
    pub async fn try_get_reply(&self, request: &M::Request) -> Result<M::Reply, RpcError> {
        self.attempt(request)?.await
    }

    /// [`try_get_reply`](Self::try_get_reply) with a deadline on provider
    /// time.
    ///
    /// When the deadline passes first the call is abandoned and fails with
    /// [`ErrorReason::Timeout`]: [`Execution::NotAdmitted`] if the request
    /// had not begun to leave this process, [`Execution::MaybeExecuted`]
    /// otherwise.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`], including the timeout.
    pub async fn try_get_reply_within(
        &self,
        request: &M::Request,
        timeout: Duration,
    ) -> Result<M::Reply, RpcError> {
        self.attempt(request)?.within(timeout).await
    }

    /// Fire and forget: queue the request once and return.
    ///
    /// The server executes it zero or one times and never answers (a
    /// one-way request is never rejected back either). `Ok` only means the
    /// request was handed to a connection or, for a local endpoint, to
    /// admission; it proves nothing about execution. Nothing is retained or
    /// retransmitted.
    ///
    /// # Errors
    ///
    /// Local refusals, all [`Execution::NotAdmitted`]: encoding, frame
    /// limit, a full request queue, a connection already closed, an
    /// endpoint the failure monitor knows is gone, shutdown.
    pub fn send(&self, request: &M::Request) -> Result<(), RpcError> {
        let (body, identity) = self.encode(request)?;
        self.rpc
            .upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?
            .send_one_way(&self.target.endpoint(), identity, body)
    }

    /// Reliable delivery: keep the request while waiting and send it again
    /// on every new connection until an outcome arrives
    /// (`FoundationDB`'s `getReply`).
    ///
    /// The server may execute the request **more than once**: every time a
    /// connection that carried it ends before the reply came back, it is
    /// queued again on the next one. The first outcome wins; the retained
    /// request is released by that outcome, by dropping the future, or by
    /// shutdown (which never retracts bytes already sent or undoes an
    /// execution). A dynamic endpoint the server declares gone
    /// ([`ErrorReason::EndpointNotFound`], [`ErrorReason::StaleIncarnation`])
    /// ends the call at once, with [`Execution::MaybeExecuted`] if an
    /// earlier copy may have run: a dead incarnation is never replaced by a
    /// fresh one. Without a failure bound the call waits as long as the
    /// server is unreachable; see
    /// [`get_reply_unless_failed_for`](Self::get_reply_unless_failed_for).
    ///
    /// # Errors
    ///
    /// Any explicit outcome other than a reply (rejections, broken promise,
    /// reply problems), a terminal endpoint failure, or shutdown.
    pub async fn get_reply(&self, request: &M::Request) -> Result<M::Reply, RpcError> {
        self.start(request, Delivery::Reliable)?.await
    }

    /// Reliable delivery bounded by observed failure
    /// (`FoundationDB`'s `getReplyUnlessFailedFor`).
    ///
    /// Like [`get_reply`](Self::get_reply), but gives up with
    /// [`ErrorReason::PeerFailed`] once the failure monitor reports the
    /// endpoint failed permanently or its address failed continuously for
    /// `sustained` plus `slope` times the time already waited (see
    /// [`FailureMonitor::on_failed_for`](crate::FailureMonitor::on_failed_for)).
    /// The error keeps the execution ambiguity of every copy sent so far.
    /// Sustained failure is an observation, not a deadline and not proof
    /// that the server is gone.
    ///
    /// # Errors
    ///
    /// As for [`get_reply`](Self::get_reply), plus
    /// [`ErrorReason::PeerFailed`].
    pub async fn get_reply_unless_failed_for(
        &self,
        request: &M::Request,
        sustained: Duration,
        slope: f64,
    ) -> Result<M::Reply, RpcError> {
        let monitor = self
            .rpc
            .failure_monitor()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?;
        let failed = monitor.on_failed_for(self.target.endpoint(), sustained, slope);
        let mut call = self.start(request, Delivery::Reliable)?;
        futures::pin_mut!(failed);
        match futures::future::select(&mut call, failed).await {
            futures::future::Either::Left((outcome, _)) => outcome,
            futures::future::Either::Right((Err(error), _)) => {
                // Shutdown: the call's own completion says what it knows.
                let execution = call.cancel();
                Err(RpcError::new(error.reason().clone(), execution))
            }
            futures::future::Either::Right((Ok(()), _)) => {
                let execution = call.cancel();
                Err(RpcError::new(ErrorReason::PeerFailed, execution))
            }
        }
    }
}

/// One started call, as a future of its single completion.
///
/// Returned by [`ServiceClient::attempt`]; also what
/// [`get_reply`](ServiceClient::get_reply) waits on. Polling it to
/// completion consumes the outcome. Dropping it early releases the reply
/// route (and a reliable call's retained request): a reply that arrives
/// afterwards is counted and discarded, and a request still queued behind
/// the handshake is withdrawn.
#[must_use = "dropping an attempt abandons it"]
pub struct ReplyAttempt<P: Providers, M: RpcMethod> {
    receiver: oneshot::Receiver<Result<ReplyBytes, RpcError>>,
    guard: Option<CallGuard>,
    time: P::Time,
    _method: PhantomData<fn() -> M>,
}

impl<P: Providers, M: RpcMethod> std::fmt::Debug for ReplyAttempt<P, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyAttempt")
            .field("method", &M::NAME)
            .field("pending", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

impl<P: Providers, M: RpcMethod> ReplyAttempt<P, M> {
    fn finish(
        outcome: Result<Result<ReplyBytes, RpcError>, oneshot::Canceled>,
    ) -> Result<M::Reply, RpcError> {
        let (codec, bytes) = outcome.map_err(|oneshot::Canceled| {
            // The runtime was dropped with the call pending.
            RpcError::new(ErrorReason::Shutdown, Execution::MaybeExecuted)
        })??;
        let expected = <M::Reply as Wire>::CODEC;
        if codec != expected {
            return Err(RpcError::new(
                ErrorReason::CodecMismatch {
                    sent: codec,
                    expected,
                },
                Execution::Executed,
            ));
        }
        <M::Reply as Wire>::decode(&bytes).map_err(|error| {
            RpcError::new(ErrorReason::MalformedReply(error.0), Execution::Executed)
        })
    }

    /// Whether the request (any copy of it, for a reliable call) began to
    /// leave this process, so the server may have executed it; `None` once
    /// the outcome arrived.
    #[must_use]
    pub fn transmitted(&self) -> Option<bool> {
        self.guard.as_ref().and_then(CallGuard::transmitted)
    }

    /// Stop waiting and report what is known about execution: withdrawn
    /// before it left ([`Execution::NotAdmitted`]), possibly executed
    /// ([`Execution::MaybeExecuted`]), or, if the outcome had already
    /// arrived, what that outcome proved.
    pub fn cancel(mut self) -> Execution {
        if let Some(guard) = self.guard.take()
            && let Some(transmitted) = guard.expire()
        {
            return if transmitted {
                Execution::MaybeExecuted
            } else {
                Execution::NotAdmitted
            };
        }
        // Completed in the same instant, or the runtime is gone: the
        // receiver holds the answer either way.
        match self.receiver.try_recv() {
            Ok(Some(Ok(_))) => Execution::Executed,
            Ok(Some(Err(error))) => error.execution(),
            Ok(None) | Err(oneshot::Canceled) => Execution::MaybeExecuted,
        }
    }

    /// Wait at most `timeout` of provider time; on expiry the attempt is
    /// abandoned and fails with [`ErrorReason::Timeout`] carrying what
    /// [`cancel`](Self::cancel) knows.
    ///
    /// # Errors
    ///
    /// The attempt's own error, or the timeout.
    pub async fn within(mut self, timeout: Duration) -> Result<M::Reply, RpcError> {
        let time = self.time.clone();
        let sleep = time.sleep(timeout);
        futures::pin_mut!(sleep);
        match futures::future::select(&mut self, sleep).await {
            futures::future::Either::Left((outcome, _)) => outcome,
            futures::future::Either::Right(_) => {
                if let Some(guard) = self.guard.take()
                    && let Some(transmitted) = guard.expire()
                {
                    return Err(RpcError::new(
                        ErrorReason::Timeout,
                        if transmitted {
                            Execution::MaybeExecuted
                        } else {
                            Execution::NotAdmitted
                        },
                    ));
                }
                // Completed in the same instant, or the runtime is gone.
                Self::finish((&mut self.receiver).await)
            }
        }
    }
}

// No field is ever pinned: the receiver is `Unpin` and the clock is only
// used to create a separate sleep future.
impl<P: Providers, M: RpcMethod> Unpin for ReplyAttempt<P, M> {}

impl<P: Providers, M: RpcMethod> Future for ReplyAttempt<P, M> {
    type Output = Result<M::Reply, RpcError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::task::ready!(Pin::new(&mut self.receiver).poll(cx));
        if let Some(guard) = self.guard.take() {
            guard.disarm();
        }
        Poll::Ready(Self::finish(outcome))
    }
}

/// Releases a pending call's reply route if the caller stops waiting.
pub(crate) struct CallGuard {
    owner: Weak<dyn CallOwner>,
    call_id: u64,
    armed: bool,
}

/// Forgets abandoned calls: implemented by the runtime's shared state.
pub(crate) trait CallOwner: Send + Sync {
    /// Forget a pending call; `Some(transmitted)` if it was still pending.
    fn abandon(&self, call_id: u64) -> Option<bool>;

    /// Whether a pending call may have left; `None` once it completed.
    fn transmitted(&self, call_id: u64) -> Option<bool>;
}

impl CallGuard {
    pub(crate) fn new(owner: Weak<dyn CallOwner>, call_id: u64) -> Self {
        Self {
            owner,
            call_id,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }

    fn transmitted(&self) -> Option<bool> {
        self.owner.upgrade()?.transmitted(self.call_id)
    }

    /// Abandon now and report whether the request had begun transmission;
    /// `None` when the call already completed or the runtime is gone.
    fn expire(mut self) -> Option<bool> {
        self.armed = false;
        self.owner.upgrade()?.abandon(self.call_id)
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(owner) = self.owner.upgrade()
        {
            let _ = owner.abandon(self.call_id);
        }
    }
}
