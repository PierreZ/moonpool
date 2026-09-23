//! Reply streams: one request, then an ordered stream of typed replies
//! paced by the consumer's consumption (`FoundationDB`'s `getReplyStream`,
//! `ReplyPromiseStream` and `AcknowledgementReceiver`).
//!
//! ```text
//!  caller                                         server
//!  get_reply_stream(req) ── REQUEST (stream, window W) ──► admission
//!                                                    handler: reply.into_stream()
//!  ReplyStream::next()  ◄── STREAM_ITEM #0, #1, ... ──  StreamProducer::send(item)
//!    (item taken)        ── STREAM_ACK consumed ────►    (waits while W is used up)
//!                       ◄── STREAM_END (ok | error) ──  finish() / fail(code) / drop
//!  drop(stream)          ── STREAM_CANCEL ──────────►   send() -> Err(Cancelled)
//! ```
//!
//! # Contracts
//!
//! - **One registration attempt.** Opening a stream sends one request, at
//!   most once; it is never retransmitted, and a stream is never resumed
//!   on another connection. A broken connection ends the stream on both
//!   sides ([`ErrorReason::Disconnected`](crate::ErrorReason::Disconnected)
//!   for the consumer, [`SendError::Disconnected`] for the producer);
//!   continuing is the application's decision, with a new stream.
//! - **Credit is consumption.** The consumer announces a window `W`
//!   ([`StreamPolicy::window_bytes`](crate::StreamPolicy::window_bytes)).
//!   The producer's [`send`](StreamProducer::send) reserves an item's
//!   accounted size (its whole frame,
//!   [`stream_item_frame_len`](crate::protocol::stream_item_frame_len))
//!   atomically, first come first served, and waits while `W` bytes are
//!   sent and unacknowledged. The consumer acknowledges an item when its
//!   application takes it: on arrival if the application is already
//!   waiting (the item is handed to it), otherwise when it is popped from
//!   the queue. Bytes read off the socket earn nothing, so a slow consumer
//!   holds at most `W` bytes and a stalled one stops the producer.
//!   Counters are cumulative, checked `u64`s.
//! - **Oversized items are refused, never parked.** An item larger than
//!   the window or the session's frame limit fails its `send` with
//!   [`SendError::TooLarge`] at once; the stream stays open. Choose a
//!   window at least as large as the largest item.
//! - **Order and one terminal state.** Items carry sequence numbers; the
//!   consumer checks them, the announced item count at the end and its
//!   window, and ends the stream with
//!   [`ErrorReason::StreamProtocol`](crate::ErrorReason::StreamProtocol)
//!   (telling the producer to stop) on any violation instead of skipping.
//!   The producer checks every acknowledgement: a repeat is ignored; one
//!   that regresses, exceeds what was sent, or does not land on an item
//!   boundary ends the stream with a protocol error. Every stream ends
//!   exactly once, and **every item that reached the consumer is observable
//!   in order before its terminal outcome**, whatever that outcome is
//!   (normal end, the producer's [`fail`](StreamProducer::fail), a broken
//!   promise, a disconnect). A consumer that drops its stream observes
//!   nothing more; the producer's unwritten items are discarded.
//! - **Cancellation does not roll back.** Dropping a [`ReplyStream`]
//!   before the request left withdraws it (never admitted); after, sends
//!   `STREAM_CANCEL`, and the producer's next `send` (or the one waiting
//!   for credit) fails with [`SendError::Cancelled`]. Effects the handler
//!   already had stay.
//! - **Errors need no credit.** A stream's end is not an item: it is
//!   emitted even when the window is exhausted, after the items already
//!   queued.
//!
//! What an error proves about execution: a rejection before admission is
//! [`Execution::NotAdmitted`](crate::Execution::NotAdmitted); once an item
//! arrived the handler demonstrably ran, so every later error is
//! [`Execution::Executed`](crate::Execution::Executed); before any item, a
//! disconnect or broken promise is
//! [`Execution::MaybeExecuted`](crate::Execution::MaybeExecuted).

pub(crate) mod consumer;
pub(crate) mod credit;
pub(crate) mod producer;

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use self::consumer::ConsumerCore;
use self::producer::StreamCore;
use crate::call::client::CallGuard;
use crate::codec::{CodecId, Wire, encode_to_vec};
use crate::error::{ErrorReason, Execution, RpcError};
use crate::protocol::{RpcMethod, WireError, stream_item_frame_len};
use crate::stats::Counters;
use crate::transport::upgrade::PeerContext;

/// Why a [`StreamProducer`] could not send.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// The consumer abandoned the stream. Stop producing; nothing more
    /// will be delivered.
    #[error("the consumer cancelled the stream")]
    Cancelled,
    /// The connection to the consumer ended; the stream is over and is
    /// never resumed.
    #[error("the connection to the consumer ended")]
    Disconnected,
    /// The RPC runtime shut down.
    #[error("the RPC runtime shut down")]
    Shutdown,
    /// The item's accounted size exceeds the stream's window or the
    /// session's frame limit: it can never be sent. Nothing was sent and
    /// the stream stays open.
    #[error("an item of {size} bytes exceeds the {limit}-byte limit")]
    TooLarge {
        /// The item's accounted size.
        size: u64,
        /// The smaller of the window and the frame limit.
        limit: u64,
    },
    /// The item could not be encoded. Nothing was sent and the stream
    /// stays open.
    #[error("item encoding failed: {0}")]
    Encode(String),
    /// The stream already ended.
    #[error("the stream already ended")]
    Ended,
    /// The consumer broke the stream protocol (or a counter would
    /// overflow); the stream was ended with an error.
    #[error("stream protocol violation: {0}")]
    Protocol(String),
}

/// The producing side of one reply stream, from
/// [`ReplyHandle::into_stream`](crate::ReplyHandle::into_stream).
///
/// [`send`](Self::send) items in order, then [`finish`](Self::finish) (a
/// normal end) or [`fail`](Self::fail) with an application code. Dropping
/// the producer without either ends the stream as a broken promise
/// ([`ErrorReason::BrokenPromise`](crate::ErrorReason::BrokenPromise)).
/// Every item already sent is delivered before the end, and the end needs
/// no credit.
///
/// `send` takes `&self`: several tasks may send concurrently through one
/// producer; credit is reserved atomically and in arrival order, so they
/// can never oversubscribe the window, and items leave in the order their
/// credit was reserved.
#[must_use = "dropping a stream producer ends the stream as a broken promise"]
pub struct StreamProducer<M: RpcMethod> {
    core: Arc<StreamCore>,
    ended: bool,
    peer: Option<PeerContext>,
    counters: Arc<Counters>,
    _method: PhantomData<fn(M::Reply)>,
}

impl<M: RpcMethod> StreamProducer<M> {
    pub(crate) fn new(
        core: Arc<StreamCore>,
        peer: Option<PeerContext>,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            core,
            ended: false,
            peer,
            counters,
            _method: PhantomData,
        }
    }

    fn encode(item: &M::Reply) -> Result<(CodecId, Vec<u8>, u64), SendError> {
        let body = encode_to_vec(item).map_err(|error| SendError::Encode(error.0))?;
        let size = stream_item_frame_len(body.len());
        Ok((<M::Reply as Wire>::CODEC, body, size))
    }

    /// Send one item, waiting for credit.
    ///
    /// Resolves once the item is queued for the consumer, which is not a
    /// delivery acknowledgement. Waits while the consumer's window is used
    /// up (sent, not yet consumed). Cancelling the future gives up its
    /// place in line; nothing was sent.
    ///
    /// # Errors
    ///
    /// [`SendError::TooLarge`] and [`SendError::Encode`] leave the stream
    /// open; every other error means the stream is over.
    pub async fn send(&self, item: &M::Reply) -> Result<(), SendError> {
        let (codec, body, size) = Self::encode(item)?;
        Send {
            core: &self.core,
            ticket: None,
            item: Some((codec, body)),
            size,
        }
        .await
    }

    /// Send one item only if credit is free right now and no other send
    /// waits for it: `Ok(true)` if it was queued, `Ok(false)` otherwise.
    ///
    /// # Errors
    ///
    /// As for [`send`](Self::send).
    pub fn try_send(&self, item: &M::Reply) -> Result<bool, SendError> {
        let (codec, body, size) = Self::encode(item)?;
        let mut send = Send {
            core: &self.core,
            ticket: None,
            item: Some((codec, body)),
            size,
        };
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        match Pin::new(&mut send).poll(&mut cx) {
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(error)) => Err(error),
            Poll::Pending => Ok(false),
        }
    }

    /// Wait until some credit is free (`FoundationDB`'s `onReady`): the
    /// consumer took enough that the window is not used up and no send is
    /// waiting. It reserves nothing; a following `send` of a large item
    /// may still wait.
    ///
    /// # Errors
    ///
    /// The error that ended the stream.
    pub async fn ready(&self) -> Result<(), SendError> {
        futures::future::poll_fn(|cx| self.core.poll_ready(cx)).await
    }

    /// End the stream normally, after every item already sent. Returns
    /// whether the end was emitted (`false` if the stream was already
    /// over: cancelled or disconnected).
    #[must_use = "`false` means the stream was already over"]
    pub fn finish(mut self) -> bool {
        self.ended = true;
        self.core.end(None)
    }

    /// End the stream with an application error `code`, after every item
    /// already sent; the consumer gets
    /// [`ErrorReason::StreamFailed`](crate::ErrorReason::StreamFailed).
    /// Returns whether the end was emitted.
    #[must_use = "`false` means the stream was already over"]
    pub fn fail(mut self, code: u64) -> bool {
        self.ended = true;
        self.core.end(Some(WireError::StreamFailed { code }))
    }

    /// End with a transport error status (a one-item reply that could not
    /// be sent).
    pub(crate) fn end_with(mut self, error: WireError) -> bool {
        self.ended = true;
        self.core.end(Some(error))
    }

    /// The window in force: the smaller of the consumer's announcement,
    /// this runtime's [`max_window_bytes`](crate::StreamPolicy::max_window_bytes)
    /// and any [`limit_window`](Self::limit_window).
    #[must_use]
    pub fn window(&self) -> u64 {
        self.core.window()
    }

    /// Lower the window (`FoundationDB`'s `setByteLimit`); it can never be
    /// raised above what the consumer announced.
    pub fn limit_window(&self, bytes: u64) {
        self.core.limit_window(bytes);
    }

    /// Items sent so far.
    #[must_use]
    pub fn items_sent(&self) -> u64 {
        self.core.items()
    }

    /// Accounted bytes sent and not yet consumed.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.core.in_flight()
    }

    /// Whether the stream is over (ended, cancelled, disconnected).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.core.terminal().is_some()
    }

    /// What the session upgrade established about the consumer's peer, or
    /// `None` for a consumer in this runtime.
    #[must_use]
    pub fn peer(&self) -> Option<&PeerContext> {
        self.peer.as_ref()
    }
}

impl<M: RpcMethod> Drop for StreamProducer<M> {
    fn drop(&mut self) {
        if !self.ended && self.core.end(Some(WireError::BrokenPromise)) {
            Counters::bump(&self.counters.broken_promises);
        }
    }
}

impl<M: RpcMethod> std::fmt::Debug for StreamProducer<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamProducer")
            .field("method", &M::NAME)
            .field("items_sent", &self.core.items())
            .finish_non_exhaustive()
    }
}

/// One send waiting for credit; leaves its place in line when dropped.
struct Send<'a> {
    core: &'a Arc<StreamCore>,
    ticket: Option<u64>,
    item: Option<(CodecId, Vec<u8>)>,
    size: u64,
}

impl Future for Send<'_> {
    type Output = Result<(), SendError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.core
            .poll_send(cx, &mut this.ticket, &mut this.item, this.size)
    }
}

impl Drop for Send<'_> {
    fn drop(&mut self) {
        self.core.abandon_send(&mut self.ticket);
    }
}

/// The consuming side of one reply stream, from
/// [`ServiceClient::get_reply_stream`](crate::ServiceClient::get_reply_stream).
///
/// A [`Stream`](futures::Stream) of `Result<M::Reply, RpcError>`: the
/// items in order, then `None` after a normal end, or one `Err` with the
/// terminal error and then `None`. Taking an item acknowledges it to the
/// producer, which returns its credit. Dropping the stream abandons it
/// (see the [module docs](crate::stream)).
#[must_use = "dropping a reply stream abandons it"]
pub struct ReplyStream<M: RpcMethod> {
    core: Arc<ConsumerCore>,
    guard: Option<CallGuard>,
    _method: PhantomData<fn() -> M>,
}

impl<M: RpcMethod> ReplyStream<M> {
    pub(crate) fn new(core: Arc<ConsumerCore>, guard: CallGuard) -> Self {
        Self {
            core,
            guard: Some(guard),
            _method: PhantomData,
        }
    }

    /// The next item, the terminal error, or `None` once over.
    pub async fn recv(&mut self) -> Option<Result<M::Reply, RpcError>> {
        futures::StreamExt::next(self).await
    }

    /// The window this stream announced.
    #[must_use]
    pub fn window(&self) -> u64 {
        self.core.window()
    }

    /// Accounted bytes received and not yet taken.
    #[must_use]
    pub fn buffered_bytes(&self) -> u64 {
        self.core.buffered()
    }

    /// Items received so far (taken or buffered).
    #[must_use]
    pub fn received(&self) -> u64 {
        self.core.received()
    }

    /// Whether the terminal outcome is known (items may still be buffered).
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        self.core.is_terminated()
    }

    /// Abandon the stream and report what is known about execution: the
    /// request was withdrawn before it left ([`Execution::NotAdmitted`]),
    /// may have reached the handler ([`Execution::MaybeExecuted`]), or an
    /// item already proved it ran ([`Execution::Executed`]).
    pub fn cancel(mut self) -> Execution {
        let transmitted = self.guard.take().and_then(CallGuard::expire);
        if self.core.received() > 0 {
            return Execution::Executed;
        }
        match transmitted {
            Some(false) => Execution::NotAdmitted,
            _ => Execution::MaybeExecuted,
        }
    }

    fn decode(&mut self, codec: CodecId, bytes: &[u8]) -> Result<M::Reply, RpcError> {
        let expected = <M::Reply as Wire>::CODEC;
        let error = if codec == expected {
            match <M::Reply as Wire>::decode(bytes) {
                Ok(item) => return Ok(item),
                Err(error) => {
                    RpcError::new(ErrorReason::MalformedReply(error.0), Execution::Executed)
                }
            }
        } else {
            RpcError::new(
                ErrorReason::CodecMismatch {
                    sent: codec,
                    expected,
                },
                Execution::Executed,
            )
        };
        // An item that does not decode ends the stream: order forbids
        // skipping it. The producer is told to stop.
        self.core.fail_locally(error.clone());
        if let Some(guard) = self.guard.take() {
            let _ = guard.expire();
        }
        Err(error)
    }
}

impl<M: RpcMethod> Unpin for ReplyStream<M> {}

impl<M: RpcMethod> futures::Stream for ReplyStream<M> {
    type Item = Result<M::Reply, RpcError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match std::task::ready!(self.core.poll_next(cx)) {
            Some(Ok((codec, bytes))) => Poll::Ready(Some(self.decode(codec, &bytes))),
            Some(Err(error)) => Poll::Ready(Some(Err(error))),
            None => Poll::Ready(None),
        }
    }
}

impl<M: RpcMethod> std::fmt::Debug for ReplyStream<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyStream")
            .field("method", &M::NAME)
            .field("window", &self.core.window())
            .field("received", &self.core.received())
            .finish_non_exhaustive()
    }
}
