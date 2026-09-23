//! Frames on a live session: the handshake, peer selection for accepted
//! sessions, liveness and idleness.

use std::net::SocketAddr;
use std::sync::Arc;

use moonpool_core::Providers;

use super::connection::{CloseReason, Connection, Direction, PeerHello};
use super::peer::Peer;
use super::{Admission, Origin, Shared, State};
use crate::call::reply::{Outstanding, ReplyRoute};
use crate::config::{InboundSharing, MIN_FRAME_BYTES};
use crate::error::CallIdentity;
use crate::protocol::{
    CREDENTIALS_VERSION, PROTOCOL_MAGIC, REQUEST_FLAG_ONE_WAY, REQUEST_FLAG_STREAM, WireMessage,
    encode_frame, encode_message, negotiate,
};
use crate::stats::Counters;
use crate::stream::consumer::AckRoute;

/// The fields of a peer's `Hello`.
#[derive(Clone, Copy)]
struct Hello {
    magic: u32,
    versions: (u16, u16),
    incarnation: crate::endpoint::Incarnation,
    features: u64,
    max_frame_bytes: u32,
    listen: Option<SocketAddr>,
}

impl<P: Providers> Shared<P> {
    /// Handle one decoded frame from `connection`.
    pub(super) fn on_message(
        &self,
        connection: &Arc<Connection>,
        message: WireMessage,
    ) -> Result<(), CloseReason> {
        let now = self.now();
        connection.note_received(now);
        let established = connection.is_established();
        match message {
            WireMessage::Hello {
                magic,
                min_version,
                max_version,
                incarnation,
                features,
                max_frame_bytes,
                listen,
            } => {
                if established {
                    return Err(CloseReason::Protocol("duplicate handshake".into()));
                }
                self.on_hello(
                    connection,
                    Hello {
                        magic,
                        versions: (min_version, max_version),
                        incarnation,
                        features,
                        max_frame_bytes,
                        listen,
                    },
                )
            }
            _ if !established => Err(CloseReason::Protocol("frame before handshake".into())),
            request @ WireMessage::Request { .. } => {
                connection.note_used(now);
                self.on_request(connection, request)
            }
            WireMessage::Reply { call_id, outcome } => {
                connection.note_used(now);
                self.complete(Origin::Connection(connection.id()), call_id, outcome);
                Ok(())
            }
            WireMessage::Ping { nonce } => {
                let pong = encode_frame(&encode_message(&WireMessage::Pong { nonce }), u32::MAX)
                    .unwrap_or_default();
                let _ = connection.push_control(pong);
                Ok(())
            }
            // Any frame is liveness; the pong carries nothing else.
            WireMessage::Pong { .. } => Ok(()),
            stream => {
                self.on_stream_frame(connection, stream, now);
                Ok(())
            }
        }
    }

    /// A request frame: build its reply context and present it for
    /// admission.
    fn on_request(
        &self,
        connection: &Arc<Connection>,
        request: WireMessage,
    ) -> Result<(), CloseReason> {
        let WireMessage::Request {
            call_id,
            incarnation,
            token,
            interface,
            interface_version,
            method,
            schema,
            codec,
            flags,
            stream_window,
            metadata,
            body,
        } = request
        else {
            return Ok(());
        };
        let stream = flags & REQUEST_FLAG_STREAM != 0;
        if stream && connection.has_stream(call_id) {
            return Err(CloseReason::Protocol(format!(
                "stream id {call_id} reused while live"
            )));
        }
        let context = if flags & REQUEST_FLAG_ONE_WAY == 0 {
            self.context(
                ReplyRoute::Remote {
                    connection: Arc::downgrade(connection),
                    call_id,
                },
                connection.peer_context(),
                Some(Outstanding::remote(connection, &self.counters)),
            )
        } else {
            Counters::bump(&self.counters.one_way_received);
            self.context(ReplyRoute::Discard, connection.peer_context(), None)
        };
        // Credentials count only on a session that negotiated them; version
        // 1 carries the section and ignores it, as version 1 specifies.
        let carries_credentials = connection
            .peer_hello()
            .is_some_and(|hello| hello.version >= CREDENTIALS_VERSION);
        self.admit(
            &Admission {
                incarnation,
                token,
                identity: CallIdentity {
                    interface: (interface, interface_version),
                    method,
                    schema,
                    codec,
                },
                stream_window: stream.then_some(stream_window),
                metadata: carries_credentials.then_some(metadata.as_slice()),
                body: &body,
            },
            context,
        );
        Ok(())
    }

    /// A reply stream frame, in either direction.
    fn on_stream_frame(
        &self,
        connection: &Arc<Connection>,
        message: WireMessage,
        now: std::time::Duration,
    ) {
        match message {
            WireMessage::StreamItem {
                call_id,
                sequence,
                codec,
                body,
            } => {
                connection.note_used(now);
                let route = AckRoute::Remote(Arc::downgrade(connection));
                self.stream_item(
                    Origin::Connection(connection.id()),
                    &route,
                    call_id,
                    sequence,
                    codec,
                    body,
                );
            }
            WireMessage::StreamEnd {
                call_id,
                items,
                error,
            } => {
                connection.note_used(now);
                self.stream_end(Origin::Connection(connection.id()), call_id, items, error);
            }
            WireMessage::StreamAck { call_id, consumed } => {
                self.stream_ack(connection, call_id, consumed);
            }
            WireMessage::StreamCancel { call_id } => {
                self.stream_cancel(connection, call_id);
            }
            // Handled by `on_message`.
            WireMessage::Hello { .. }
            | WireMessage::Request { .. }
            | WireMessage::Reply { .. }
            | WireMessage::Ping { .. }
            | WireMessage::Pong { .. } => {}
        }
    }

    /// The peer's handshake: check it, settle peer selection, establish.
    fn on_hello(&self, connection: &Arc<Connection>, hello: Hello) -> Result<(), CloseReason> {
        let Hello {
            magic,
            versions: (min_version, max_version),
            incarnation,
            features,
            max_frame_bytes,
            listen,
        } = hello;
        if magic != PROTOCOL_MAGIC {
            return Err(CloseReason::Protocol(format!("bad magic {magic:#x}")));
        }
        let ours = self.config.advertised_versions();
        let Some(version) = negotiate(ours, (min_version, max_version)) else {
            return Err(CloseReason::Version(format!(
                "peer speaks {min_version}..={max_version}, this runtime {}..={}",
                ours.0, ours.1
            )));
        };
        if max_frame_bytes < MIN_FRAME_BYTES {
            return Err(CloseReason::Protocol(format!(
                "peer frame limit {max_frame_bytes} below {MIN_FRAME_BYTES}"
            )));
        }
        // Settle selection before establishing, so a losing session never
        // lets a request through.
        if connection.direction() == Direction::Inbound
            && let Some(listen) = listen
        {
            self.select_inbound(connection, listen);
        }
        connection.establish(
            PeerHello {
                incarnation,
                version,
                max_frame_bytes,
            },
            self.now(),
        );
        if let Some(address) = connection.peer_address() {
            let changed = {
                let mut state = self.lock();
                if let Some(peer) = state.peers.get_mut(&address) {
                    peer.established();
                }
                state.monitor.connected(address)
            };
            if changed {
                self.watch.notify();
            }
        }
        tracing::debug!(
            peer = %connection.peer(),
            %incarnation,
            version,
            features,
            max_frame_bytes,
            ?listen,
            "rpc session established"
        );
        Ok(())
    }

    /// An accepted session announced it listens at `listen`: decide
    /// whether it becomes this runtime's selected connection to that
    /// address.
    ///
    /// Only when sharing is on, the claim passes the [`InboundSharing`]
    /// check, and the peer table has room. Then, following `FoundationDB`'s
    /// `Peer::onIncomingConnection`: with no selected connection, adopt it;
    /// the selected connection is an earlier accepted session (the peer
    /// dialed again), replace it; the selected connection is this
    /// runtime's own dial, adopt when `listen` is larger than this
    /// runtime's address (the larger address keeps the connection *it*
    /// dialed, so the smaller side gives its own up), or when that dial has
    /// failed to establish for [`PeerPolicy::always_accept_after`]
    /// (`ALWAYS_ACCEPT_DELAY`: a peer that can dial us but that we cannot
    /// dial stays reachable). Otherwise the accepted session is only served.
    ///
    /// Unlike `FoundationDB`, the larger side never closes the other's dial:
    /// the smaller side closes it itself once it adopted the larger side's
    /// dial. A peer that cannot or will not adopt (sharing off, an
    /// unverified claim, a full table) is therefore never locked out; the
    /// pair just keeps one connection per direction. Queued, never-written
    /// requests move to the adopted connection; anything already written
    /// rides the replaced one down.
    ///
    /// [`InboundSharing`]: crate::InboundSharing
    /// [`PeerPolicy::always_accept_after`]: crate::PeerPolicy::always_accept_after
    fn select_inbound(&self, connection: &Arc<Connection>, listen: SocketAddr) {
        let Some(local) = self.address else {
            return;
        };
        if listen == local {
            return;
        }
        match self.config.peer.share_inbound_sessions {
            InboundSharing::Disabled => return,
            InboundSharing::Trusted => {}
            InboundSharing::SameIp => {
                let observed = connection
                    .peer()
                    .parse::<SocketAddr>()
                    .ok()
                    .map(|address| address.ip());
                if observed != Some(listen.ip()) {
                    Counters::bump(&self.counters.unverified_listen_addresses);
                    tracing::debug!(
                        %listen,
                        peer = %connection.peer(),
                        "rpc accepted session claims another host's address; served only"
                    );
                    return;
                }
            }
        }
        let now = self.now();
        let mut state = self.lock();
        if !state.peers.contains_key(&listen)
            && state.peers.len() >= self.config.peer.max_tracked_addresses
        {
            state.peers.retain(|_, peer| !peer.is_forgettable(now));
            if state.peers.len() >= self.config.peer.max_tracked_addresses {
                drop(state);
                Counters::bump(&self.counters.peer_table_full);
                tracing::debug!(%listen, "rpc peer table full; accepted session served only");
                return;
            }
        }
        let always_accept = self.config.peer.always_accept_after;
        let peer = state
            .peers
            .entry(listen)
            .or_insert_with(|| Peer::new(&self.config.peer));
        let stalled = peer.dialing_stalled(now, always_accept);
        let replaced = match peer.live() {
            None => None,
            Some(current) if current.direction() == Direction::Inbound => Some(Arc::clone(current)),
            Some(current) if listen > local => Some(Arc::clone(current)),
            Some(current) if stalled && !current.is_established() => {
                Counters::bump(&self.counters.accepted_over_stalled_dial);
                Some(Arc::clone(current))
            }
            Some(_) => {
                // Keep our own dial, but remember this session: if our
                // dials stall, it is how we reach the peer.
                peer.candidate = Some(Arc::downgrade(connection));
                drop(state);
                tracing::debug!(%listen, %local, "rpc simultaneous connect: keeping our own dial");
                return;
            }
        };
        self.adopt(&mut state, listen, connection, replaced, now);
    }

    /// Make `connection` the selected connection to `listen`, moving the
    /// never-written requests of the connection it replaces and closing
    /// that one (its driver then reports the end and fails what it had
    /// written).
    pub(super) fn adopt(
        &self,
        state: &mut State,
        listen: SocketAddr,
        connection: &Arc<Connection>,
        replaced: Option<Arc<Connection>>,
        now: std::time::Duration,
    ) {
        connection.select_for(listen);
        if let Some(peer) = state.peers.get_mut(&listen) {
            peer.current = Some(Arc::clone(connection));
            peer.candidate = None;
            peer.adopted(now);
        }
        Counters::bump(&self.counters.adopted_connections);
        if let Some(replaced) = replaced {
            let moved = replaced.take_requests();
            for (_, call_id) in &moved {
                if let Some(call) = call_id.and_then(|call_id| state.pending.get_mut(&call_id)) {
                    call.origin = Origin::Connection(connection.id());
                }
            }
            let _ = connection.adopt_requests(moved, now);
            let _ = replaced.close(CloseReason::Replaced);
            tracing::debug!(%listen, "rpc accepted session replaces the selected connection");
        }
    }

    /// Close `connection` if it has been idle long enough at this instant.
    pub(super) fn close_if_idle(&self, connection: &Connection) -> bool {
        let now = self.now();
        let state = self.lock();
        let selected = connection.peer_address().is_some();
        if selected {
            let origin = Origin::Connection(connection.id());
            if state.pending.values().any(|call| call.origin == origin) {
                return false;
            }
        }
        let idle = if selected {
            self.config.peer.idle_timeout
        } else {
            self.config.peer.inbound_idle_timeout
        };
        // Checked and closed under the state lock: no call can be queued on
        // it in between.
        connection.close_if_idle(now, idle, selected)
    }

    /// Queue a ping on `connection`.
    pub(super) fn ping(&self, connection: &Connection) {
        let nonce = {
            let mut state = self.lock();
            state.next_ping = state.next_ping.wrapping_add(1);
            state.next_ping
        };
        let frame = encode_frame(&encode_message(&WireMessage::Ping { nonce }), u32::MAX)
            .unwrap_or_default();
        if connection.push_control(frame) {
            Counters::bump(&self.counters.pings_sent);
        }
    }
}
