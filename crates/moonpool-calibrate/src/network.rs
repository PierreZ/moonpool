//! Small-message TCP round-trip time measured against a real socket.
//!
//! Both sides are raw [`std::net`] blocking sockets timed with [`Instant`]. No
//! moonpool network provider, no simulated time, no async runtime.
//!
//! The protocol is deliberately the smallest thing that can be measured:
//!
//! ```text
//! client                          server
//!   |-- 8-byte sequence number ----->|
//!   |<-- the same 8 bytes -----------|
//!   RTT = elapsed
//! ```
//!
//! One connection carries every sample. Connection establishment is *not*
//! measured — moonpool has a separate `connect_latency` knob, and folding a
//! handshake into every sample would say nothing about steady-state message
//! delay. Bandwidth, payload-size sweeps and loss are out of scope: this is not
//! `iperf`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Instant;

use crate::stats::Latencies;

/// Wire size of one ping/pong message: a big-endian `u64` sequence number.
pub const MESSAGE_LEN: usize = 8;

/// Default port for `moonpool-calibrate network listen`.
pub const DEFAULT_PORT: u16 = 7777;

/// Serve ping/pong on `listener` until the process is stopped.
///
/// Connections are handled one at a time, in accept order: a calibration run
/// uses a single connection, and concurrency would only add scheduling noise to
/// whatever the peer is trying to measure.
///
/// # Errors
///
/// Returns an error if accepting a connection fails. Errors on an individual
/// connection are reported on stderr and the listener moves on to the next one.
pub fn serve(listener: &TcpListener) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept()?;
        eprintln!("  connection from {peer}");
        match echo_connection(stream) {
            Ok(messages) => eprintln!("  {peer} disconnected after {messages} messages"),
            Err(error) => eprintln!("  {peer} failed: {error}"),
        }
    }
}

/// Echo every complete message on one connection until the peer closes it.
///
/// Returns the number of messages echoed.
///
/// # Errors
///
/// Returns any socket error other than a clean end of stream.
pub fn echo_connection(mut stream: TcpStream) -> std::io::Result<u64> {
    // Nagle would batch these tiny messages and measure the delayed-ack timer
    // rather than the network.
    stream.set_nodelay(true)?;

    let mut message = [0_u8; MESSAGE_LEN];
    let mut echoed = 0_u64;
    loop {
        match read_full(&mut stream, &mut message)? {
            ReadOutcome::Complete => {}
            ReadOutcome::Eof => return Ok(echoed),
        }
        stream.write_all(&message)?;
        echoed += 1;
    }
}

/// Measure round-trip time to `address` over one connection.
///
/// `warmup` unrecorded round trips run first, then `samples` recorded ones. Each
/// response is verified against the sequence number that was sent.
///
/// # Errors
///
/// Returns an error if the address cannot be resolved or connected to, if the
/// socket fails, if the peer closes early, or if a response does not match the
/// request that produced it.
pub fn measure(address: &str, samples: u64, warmup: u64) -> std::io::Result<Latencies> {
    let resolved = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| invalid_data(format!("no address resolved for {address}")))?;
    let mut stream = TcpStream::connect(resolved)?;
    stream.set_nodelay(true)?;

    let mut latencies = Latencies::new("rtt");
    let mut sequence = 0_u64;

    for _ in 0..warmup {
        round_trip(&mut stream, sequence)?;
        sequence += 1;
    }
    for _ in 0..samples {
        let elapsed = round_trip(&mut stream, sequence)?;
        latencies.record(elapsed);
        sequence += 1;
    }

    Ok(latencies)
}

/// Send one sequence number, wait for the whole echo, verify it, return the RTT.
fn round_trip(stream: &mut TcpStream, sequence: u64) -> std::io::Result<std::time::Duration> {
    let request = sequence.to_be_bytes();
    let mut response = [0_u8; MESSAGE_LEN];

    let start = Instant::now();
    stream.write_all(&request)?;
    match read_full(stream, &mut response)? {
        ReadOutcome::Complete => {}
        ReadOutcome::Eof => {
            return Err(invalid_data(format!(
                "peer closed the connection while waiting for sequence {sequence}"
            )));
        }
    }
    let elapsed = start.elapsed();

    if response != request {
        return Err(invalid_data(format!(
            "echo mismatch: sent {sequence}, received {}",
            u64::from_be_bytes(response)
        )));
    }
    Ok(elapsed)
}

/// Whether [`read_full`] filled the buffer or hit a clean end of stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOutcome {
    /// The buffer was filled completely.
    Complete,
    /// The peer closed before any byte of this message arrived.
    Eof,
}

/// Fill `buffer` completely, tolerating short reads.
///
/// A clean close *between* messages is [`ReadOutcome::Eof`]; a close in the
/// middle of one is an error, because the message framing has been broken.
fn read_full(stream: &mut TcpStream, buffer: &mut [u8]) -> std::io::Result<ReadOutcome> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..])? {
            0 if filled == 0 => return Ok(ReadOutcome::Eof),
            0 => {
                return Err(invalid_data(format!(
                    "peer closed mid-message after {filled} of {} bytes",
                    buffer.len()
                )));
            }
            read => filled += read,
        }
    }
    Ok(ReadOutcome::Complete)
}

/// Build an `InvalidData` error with a message.
fn invalid_data(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PORT, MESSAGE_LEN, echo_connection, measure};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// Start a one-connection echo server on an ephemeral loopback port.
    fn spawn_echo_server() -> (String, std::thread::JoinHandle<std::io::Result<u64>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("local addr").to_string();
        let handle = std::thread::spawn(move || {
            let (stream, _peer) = listener.accept()?;
            echo_connection(stream)
        });
        (address, handle)
    }

    #[test]
    fn default_port_is_stable() {
        assert_eq!(DEFAULT_PORT, 7777);
        assert_eq!(MESSAGE_LEN, 8);
    }

    #[test]
    fn echo_returns_each_message_unchanged() {
        let (address, handle) = spawn_echo_server();
        let mut client = TcpStream::connect(&address).expect("connect");

        for sequence in 0_u64..16 {
            let request = sequence.to_be_bytes();
            client.write_all(&request).expect("write");
            let mut response = [0_u8; MESSAGE_LEN];
            client.read_exact(&mut response).expect("read");
            assert_eq!(response, request, "echo must be byte-identical");
        }

        drop(client);
        assert_eq!(handle.join().expect("server thread").expect("echo"), 16);
    }

    #[test]
    fn measure_records_the_requested_number_of_samples() {
        let (address, handle) = spawn_echo_server();

        let latencies = measure(&address, 32, 4).expect("measurement");
        assert_eq!(latencies.count(), 32);

        // Plumbing only: no assertion about how fast loopback happens to be.
        let bounds = latencies.summary().bounds();
        assert!(bounds.start <= bounds.end);

        // 4 warmup + 32 recorded round trips reached the server.
        assert_eq!(handle.join().expect("server thread").expect("echo"), 36);
    }

    #[test]
    fn measure_fails_when_the_peer_closes_early() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("local addr").to_string();
        let handle = std::thread::spawn(move || {
            let (stream, _peer) = listener.accept().expect("accept");
            drop(stream);
        });

        let error = measure(&address, 4, 0).expect_err("peer closed without echoing");
        handle.join().expect("server thread");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn measure_rejects_an_unresolvable_address() {
        assert!(measure("not-a-host-name", 1, 0).is_err());
    }
}
