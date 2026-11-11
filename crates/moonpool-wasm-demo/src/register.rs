//! The small system of the website: a replicated register (#416 (engine and
//! player), #417 (technique pages, part 1)).
//!
//! Three [`RegisterServer`] processes each keep a log of `(version, value)`
//! records in a file, one sector per version, and `sync_all` each record
//! before they answer. At boot a server keeps the newest record that passes
//! its check word. One
//! [`RegisterClient`] workload writes versions 1, 2, 3 … to all servers and
//! calls a write committed when a quorum (two of three) answered. After each
//! write it reads from the servers and keeps the highest version that a
//! quorum returned. A read quorum and a write quorum always share a server,
//! so a read sees the last committed write. [`RegisterInvariant`] checks that
//! across the processes, from the trace events alone.
//!
//! The planted bug ([`RunConfig::bug`](crate::RunConfig)): the client takes
//! the first answer of a read in place of a quorum. A server that missed the
//! last write can answer first, and the read goes back in time.
//!
//! Each fact that a picture of the site needs is an ordinary `tracing` event:
//! `msg_send` and `msg_recv` for each message, `log_written`, `log_synced`,
//! `write_applied`, `read_served`, `reply_dropped`, `server_boot` and
//! `server_stop` on a server, `write_committed`, `write_failed`,
//! `read_returned` and `read_failed` on the client. The code has no drawing
//! code.

use std::cell::Cell;
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::stream::{FuturesUnordered, StreamExt};
use moonpool_sim::{
    Invariant, NetworkProvider, OpenOptions, Process, SimContext, SimulationError,
    SimulationResult, StorageFile, StorageProvider, TcpListenerTrait, TimeProvider, TraceQuery,
    Workload, assert_always, assert_sometimes, buggify, hint,
};

/// The group name of the servers.
pub const SERVER: &str = "register";
/// The name of the client workload.
pub const CLIENT: &str = "client";
/// How many writes the client makes in one run.
pub const WRITES: u64 = 8;
/// The log of the register on each server: version `v` is sector `v - 1`.
const FILE: &str = "register.log";
/// One record of the log fills one sector, so a crash decides its fate whole.
const SECTOR: usize = 512;
/// How long the client waits for one answer.
const TIMEOUT: Duration = Duration::from_millis(400);
/// How long the client waits between two operations.
const PAUSE: Duration = Duration::from_millis(60);

/// The four kinds of message on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Write = 1,
    Read = 2,
    WriteAck = 3,
    ReadReply = 4,
}

impl Op {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Write),
            2 => Some(Self::Read),
            3 => Some(Self::WriteAck),
            4 => Some(Self::ReadReply),
            _ => None,
        }
    }

    /// The name of the message in the trace events.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::WriteAck => "write_ack",
            Self::ReadReply => "read_reply",
        }
    }
}

/// One message: what it is, its request id, and the register it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) op: Op,
    pub(crate) rid: u64,
    pub(crate) version: u64,
    pub(crate) value: u64,
}

const FRAME_LEN: usize = 33;

impl Frame {
    /// Encode with a check word, so a flipped bit makes the frame invalid.
    pub(crate) fn encode(self) -> [u8; FRAME_LEN] {
        let mut out = [0; FRAME_LEN];
        out[0] = self.op as u8;
        out[1..9].copy_from_slice(&self.rid.to_be_bytes());
        out[9..17].copy_from_slice(&self.version.to_be_bytes());
        out[17..25].copy_from_slice(&self.value.to_be_bytes());
        out[25..33].copy_from_slice(&self.check().to_be_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; FRAME_LEN]) -> Option<Self> {
        let word = |at: usize| {
            let mut b = [0; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            u64::from_be_bytes(b)
        };
        let frame = Self {
            op: Op::from_byte(bytes[0])?,
            rid: word(1),
            version: word(9),
            value: word(17),
        };
        (word(25) == frame.check()).then_some(frame)
    }

    fn check(self) -> u64 {
        !(self.rid ^ self.version.rotate_left(17) ^ self.value.rotate_left(33) ^ self.op as u64)
    }
}

/// The id of the message that answers request `rid` (a request has `2 * rid`).
fn reply_id(rid: u64) -> u64 {
    rid * 2 + 1
}

/// The register on disk: version, value and a check word.
fn encode_state(version: u64, value: u64) -> [u8; 24] {
    let mut out = [0; 24];
    out[..8].copy_from_slice(&version.to_be_bytes());
    out[8..16].copy_from_slice(&value.to_be_bytes());
    out[16..].copy_from_slice(&(!(version ^ value.rotate_left(29))).to_be_bytes());
    out
}

fn decode_state(bytes: &[u8; 24]) -> Option<(u64, u64)> {
    let word = |at: usize| {
        let mut b = [0; 8];
        b.copy_from_slice(&bytes[at..at + 8]);
        u64::from_be_bytes(b)
    };
    let (version, value) = (word(0), word(8));
    (word(16) == !(version ^ value.rotate_left(29))).then_some((version, value))
}

/// A server of the register: the system under test.
pub struct RegisterServer;

impl RegisterServer {
    /// Read the log and keep its newest valid record. A sector that a crash
    /// lost or damaged fails its check word and does not count.
    async fn load<F: StorageFile>(file: &F) -> SimulationResult<(u64, u64)> {
        let size = file.size().await.map_err(|e| io_error(&e))?;
        let mut newest = (0, 0);
        let sectors = usize::try_from(size).unwrap_or(usize::MAX) / SECTOR;
        for sector in 0..sectors {
            let mut bytes = [0; 24];
            file.read_at((sector * SECTOR) as u64, &mut bytes)
                .await
                .map_err(|e| io_error(&e))?;
            if let Some(record) = decode_state(&bytes) {
                newest = newest.max(record);
            }
        }
        Ok(newest)
    }
}

fn io_error(error: &std::io::Error) -> SimulationError {
    SimulationError::InvalidState(format!("register file: {error}"))
}

#[async_trait]
impl Process for RegisterServer {
    fn name(&self) -> &'static str {
        SERVER
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let storage = ctx.storage();
        let file = storage
            .open(FILE, OpenOptions::new().read(true).write(true).create(true))
            .await
            .map_err(|e| io_error(&e))?;
        storage.sync_dir(".").await.map_err(|e| io_error(&e))?;
        let (mut version, mut value) = Self::load(&file).await?;
        assert_sometimes!(version > 0, "a server boots with a stored value");
        tracing::info!(version, "server_boot");

        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let shutdown = ctx.shutdown().clone();
        loop {
            let accepted = moonpool_sim::select! {
                result = listener.accept() => result,
                () = shutdown.cancelled() => break,
            };
            let Ok((mut stream, peer)) = accepted else {
                continue;
            };
            let mut bytes = [0; FRAME_LEN];
            let read = moonpool_sim::select! {
                result = stream.read_exact(&mut bytes) => result,
                () = shutdown.cancelled() => break,
            };
            let Some(request) = read.ok().and_then(|()| Frame::decode(&bytes)) else {
                continue;
            };
            let peer = peer.split(':').next().unwrap_or(&peer).to_string();
            tracing::info!(msg = request.rid * 2, from = %peer, kind = request.op.label(), "msg_recv");

            let reply = match request.op {
                Op::Write => {
                    if request.version > version {
                        let sector = request.version - 1;
                        let mut record = [0; SECTOR];
                        record[..24].copy_from_slice(&encode_state(request.version, request.value));
                        let written = file
                            .write_at(sector * SECTOR as u64, &record)
                            .await
                            .map_err(|e| io_error(&e))?;
                        if written < SECTOR {
                            return Err(SimulationError::InvalidState(format!(
                                "register file: short write of {written} bytes"
                            )));
                        }
                        tracing::info!(sector, "log_written");
                        // The record is in the page cache, not on the disk.
                        hint!("register: record written, not synced").await;
                        file.sync_all().await.map_err(|e| io_error(&e))?;
                        tracing::info!(sector, "log_synced");
                        (version, value) = (request.version, request.value);
                    }
                    tracing::info!(version, "write_applied");
                    // The write is on disk and the client does not know yet.
                    hint!("register: write durable, ack not sent").await;
                    Frame {
                        op: Op::WriteAck,
                        rid: request.rid,
                        version,
                        value,
                    }
                }
                Op::Read => {
                    tracing::info!(version, "read_served");
                    Frame {
                        op: Op::ReadReply,
                        rid: request.rid,
                        version,
                        value,
                    }
                }
                Op::WriteAck | Op::ReadReply => continue,
            };
            // A lost answer: the client must live with a silent server.
            if buggify!() {
                tracing::info!(msg = reply_id(request.rid), "reply_dropped");
                continue;
            }
            tracing::info!(msg = reply_id(request.rid), to = %peer, kind = reply.op.label(), "msg_send");
            let _ = stream.write_all(&reply.encode()).await;
        }
        // A graceful reboot: the server stops serving before the kill.
        tracing::info!("server_stop");
        Ok(())
    }
}

/// The client of the register: the test driver.
pub struct RegisterClient {
    /// The planted bug: read from the first answer, not from a quorum.
    pub bug: bool,
    next_rid: u64,
}

impl RegisterClient {
    /// A client; `bug` plants the bug of the site.
    #[must_use]
    pub fn new(bug: bool) -> Self {
        Self { bug, next_rid: 1 }
    }

    /// Send one request to each server; return the first `need` answers.
    async fn round(
        &mut self,
        ctx: &SimContext,
        servers: &[String],
        op: Op,
        version: u64,
        need: usize,
    ) -> Vec<Frame> {
        let mut calls = FuturesUnordered::new();
        for server in servers {
            let rid = self.next_rid;
            self.next_rid += 1;
            let frame = Frame {
                op,
                rid,
                version,
                value: version * 10,
            };
            calls.push(call(ctx, server.clone(), frame));
        }
        // Wait for each call (each one ends at its timeout), so each answer
        // that comes back is read and shows on the picture. The decision
        // uses the first `need` answers, in the order they arrived.
        let mut answers = Vec::new();
        while let Some(answer) = calls.next().await {
            answers.extend(answer);
        }
        answers.truncate(need);
        answers
    }
}

/// One request and its answer, bounded by [`TIMEOUT`].
async fn call(ctx: &SimContext, server: String, frame: Frame) -> Option<Frame> {
    let exchange = async {
        let mut stream = ctx.network().connect(&server).await.ok()?;
        tracing::info!(msg = frame.rid * 2, to = %server, kind = frame.op.label(), "msg_send");
        stream.write_all(&frame.encode()).await.ok()?;
        let mut bytes = [0; FRAME_LEN];
        stream.read_exact(&mut bytes).await.ok()?;
        let answer = Frame::decode(&bytes).filter(|a| a.rid == frame.rid)?;
        tracing::info!(msg = reply_id(frame.rid), from = %server, kind = answer.op.label(), "msg_recv");
        Some(answer)
    };
    ctx.time().timeout(TIMEOUT, exchange).await.ok().flatten()
}

#[async_trait]
impl Workload for RegisterClient {
    fn name(&self) -> &'static str {
        CLIENT
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().ips_in_group(SERVER);
        let quorum = servers.len() / 2 + 1;
        for version in 1..=WRITES {
            let acks = self
                .round(ctx, &servers, Op::Write, version, servers.len())
                .await;
            if acks.len() >= quorum {
                assert_sometimes!(
                    acks.len() < servers.len(),
                    "a write commits without one server"
                );
                tracing::info!(version, acks = acks.len(), "write_committed");
            } else {
                assert_sometimes!(true, "a write misses its quorum");
                tracing::info!(version, acks = acks.len(), "write_failed");
            }
            let _ = ctx.time().sleep(PAUSE).await;

            let need = if self.bug { 1 } else { quorum };
            let replies = self.round(ctx, &servers, Op::Read, 0, need).await;
            if replies.len() >= need {
                let seen = replies.iter().map(|r| r.version).max().unwrap_or(0);
                let oldest = replies.iter().map(|r| r.version).min().unwrap_or(0);
                assert_sometimes!(oldest < seen, "a read sees two versions");
                tracing::info!(version = seen, replies = replies.len(), "read_returned");
            } else {
                tracing::info!(replies = replies.len(), "read_failed");
            }
            let _ = ctx.time().sleep(PAUSE).await;
        }
        Ok(())
    }
}

/// The rule of the invariant, fed with the client's events in order: a read
/// returns the last committed write or a newer one. The invariant and the
/// site's engine (which marks the first stale read) share it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    committed: u64,
}

impl Freshness {
    /// Take one event. `false` means a `read_returned` that went back in time.
    pub fn observe(&mut self, name: &str, version: u64) -> bool {
        match name {
            "write_committed" => {
                self.committed = self.committed.max(version);
                true
            }
            "read_returned" => version >= self.committed,
            _ => true,
        }
    }
}

/// The safety property of the register, checked across the processes from
/// the trace events: a read returns the last committed write or a newer one.
#[derive(Default)]
pub struct RegisterInvariant {
    rule: Cell<Freshness>,
    cursor: Cell<usize>,
    reads: Cell<usize>,
}

/// The message of the invariant's assertion.
pub const INVARIANT: &str = "a read returns the last committed write";

impl Invariant for RegisterInvariant {
    fn name(&self) -> &'static str {
        "register_reads_are_fresh"
    }

    fn observe(&self, query: &dyn TraceQuery, _sim_time_ms: u64) {
        // The client is sequential: merge its two streams in event order.
        let mut events = query.since("write_committed", &self.cursor);
        events.extend(query.since("read_returned", &self.reads));
        events.sort_by_key(|e| e.seq);
        let mut rule = self.rule.get();
        for event in events {
            let fresh = rule.observe(&event.name, event.u64("version").unwrap_or(0));
            assert_always!(fresh, "a read returns the last committed write");
        }
        self.rule.set(rule);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The trace events of the register that a run records.
pub const EVENTS: &[&str] = &[
    "server_boot",
    "server_stop",
    "log_written",
    "log_synced",
    "reply_dropped",
    "msg_send",
    "msg_recv",
    "write_applied",
    "read_served",
    "write_committed",
    "write_failed",
    "read_returned",
    "read_failed",
];

#[cfg(test)]
mod tests {
    use super::{Frame, Op, decode_state, encode_state, reply_id};

    #[test]
    fn a_frame_roundtrips_and_a_flipped_bit_is_refused() {
        let frame = Frame {
            op: Op::Write,
            rid: 7,
            version: 3,
            value: 30,
        };
        let mut bytes = frame.encode();
        assert_eq!(Frame::decode(&bytes), Some(frame));
        bytes[12] ^= 0x10;
        assert_eq!(Frame::decode(&bytes), None);
    }

    #[test]
    fn the_stored_register_roundtrips_and_damage_is_refused() {
        let mut bytes = encode_state(5, 50);
        assert_eq!(decode_state(&bytes), Some((5, 50)));
        bytes[0] ^= 1;
        assert_eq!(decode_state(&bytes), None);
        assert_eq!(decode_state(&[0; 24]), None);
    }

    #[test]
    fn a_reply_id_never_meets_a_request_id() {
        for rid in 1..100 {
            assert_eq!(reply_id(rid) % 2, 1);
            assert_ne!(reply_id(rid), rid * 2);
        }
    }
}
