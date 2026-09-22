//! Frames on a live session: the handshake, peer selection for accepted
//! sessions, liveness and idleness.

use std::net::SocketAddr;
use std::sync::Arc;

use moonpool_core::Providers;

use super::connection::{CloseReason, Connection, Direction, PeerHello};
use super::peer::Peer;
use super::{Admission, Origin, Shared};
use crate::call::reply::{Outstanding, ReplyRoute};
use crate::config::MIN_FRAME_BYTES;
use crate::error::CallIdentity;
use crate::protocol::{
    MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION, REQUEST_FLAG_ONE_WAY, WireMessage,
    encode_frame, encode_message, negotiate,
};
use crate::stats::Counters;

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

/// What to do with an accepted session that names a listen address.
enum Selection {
    /// Make it the selected connection; close the one it replaces.
    Adopt(Option<Arc<Connection>>),
    /// This side's own dial wins the tie-break: close the accepted session.
    Redundant,
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
            WireMessage::Request {
                call_id,
                incarnation,
                token,
                method,
                schema,
                codec,
                flags,
                // Reserved for request credentials (#218): carried, never
                // interpreted or handed to user code by this version.
                metadata: _,
                body,
            } => {
                connection.note_used(now);
                let route = if flags & REQUEST_FLAG_ONE_WAY == 0 {
                    ReplyRoute::Remote {
                        connection: Arc::downgrade(connection),
                        call_id,
                        _outstanding: Outstanding::new(connection),
                    }
                } else {
                    Counters::bump(&self.counters.one_way_received);
                    ReplyRoute::Discard
                };
                self.admit(
                    &Admission {
                        incarnation,
                        token,
                        identity: CallIdentity {
                            method,
                            schema,
                            codec,
                        },
                        body: &body,
                    },
                    route,
                    connection.peer_context(),
                );
                Ok(())
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
        let Some(version) = negotiate(
            (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
            (min_version, max_version),
        ) else {
            return Err(CloseReason::Version(format!(
                "peer speaks {min_version}..={max_version}, \
                 this build {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}"
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
            self.select_inbound(connection, listen)?;
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
            let changed = self.lock().monitor.connected(address);
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
    /// No selected connection yet: adopt it. The selected connection is an
    /// earlier accepted session: the peer dialed again, so the new one
    /// replaces it. The selected connection is this runtime's own dial
    /// (both sides dialed at once): the larger canonical address keeps the
    /// connection it dialed, so adopt when `listen` is larger than this
    /// runtime's address and refuse otherwise. Both sides reach the same
    /// verdict. Queued, never-written requests move to the adopted
    /// connection; anything already written rides the replaced one down.
    fn select_inbound(
        &self,
        connection: &Arc<Connection>,
        listen: SocketAddr,
    ) -> Result<(), CloseReason> {
        let Some(local) = self.address else {
            return Ok(());
        };
        if !self.config.peer.share_inbound_sessions || listen == local {
            return Ok(());
        }
        let now = self.now();
        let mut state = self.lock();
        let peer = state
            .peers
            .entry(listen)
            .or_insert_with(|| Peer::new(&self.config.peer));
        let selection = match peer.live() {
            None => Selection::Adopt(None),
            Some(current) if current.direction() == Direction::Inbound => {
                Selection::Adopt(Some(Arc::clone(current)))
            }
            Some(current) if listen > local => Selection::Adopt(Some(Arc::clone(current))),
            Some(_) => Selection::Redundant,
        };
        let Selection::Adopt(replaced) = selection else {
            drop(state);
            tracing::debug!(%listen, %local, "rpc simultaneous connect: keeping our own dial");
            return Err(CloseReason::Redundant);
        };
        connection.select_for(listen);
        peer.current = Some(Arc::clone(connection));
        peer.adopted(now);
        Counters::bump(&self.counters.adopted_connections);
        if let Some(replaced) = replaced {
            let moved = replaced.take_requests();
            for (_, call_id) in &moved {
                if let Some(call) = call_id.and_then(|call_id| state.pending.get_mut(&call_id)) {
                    call.origin = Origin::Connection(connection.id());
                }
            }
            let _ = connection.adopt_requests(moved, now);
            drop(state);
            // Its driver reports the end (and fails what it had written).
            let _ = replaced.close(CloseReason::Replaced);
            tracing::debug!(%listen, %local, "rpc accepted session replaces the selected connection");
        }
        Ok(())
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
