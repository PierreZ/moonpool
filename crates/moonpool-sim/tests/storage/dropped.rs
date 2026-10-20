//! A dropped mutation still lands.
//!
//! Dropping a storage future abandons the wait, not the I/O. On Tokio the
//! `spawn_blocking` task keeps running; on `io_uring` the kernel owns the
//! operation. So a dropped write, sync or `set_len` keeps its scheduled
//! completion: the effect reaches the file at that time, through the same
//! fault rolls as an awaited operation, and nobody is woken. The
//! `dropped_mutation_discard_probability` coin models the operation that
//! never reached the device; a discarded mutation leaves no trace. A dropped
//! read is cancelled outright.

use std::{
    future::Future,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{io::AsyncWriteExt, task::noop_waker};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{Event, SimWorld, StorageConfiguration, storage::SimStorageFile};

/// How many events a driven future may consume before it is declared stuck.
const MAX_STEPS: usize = 10_000;

const OLD: &[u8] = b"old contents....";
const NEW: &[u8] = b"NEW CONTENTS!!!!";

fn ip() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn disk(discard: f64) -> StorageConfiguration {
    StorageConfiguration {
        dropped_mutation_discard_probability: discard,
        ..StorageConfiguration::fast_local()
    }
}

fn world(seed: u64, discard: f64) -> SimWorld {
    let mut sim = SimWorld::new_with_seed(seed);
    sim.set_storage_config(disk(discard));
    sim
}

/// Poll `future` until it resolves, stepping the world between polls.
fn settle<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    for _ in 0..MAX_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(
            sim.has_pending_events(),
            "future parked with an empty world"
        );
        sim.step();
    }
    panic!("simulation-backed future exceeded {MAX_STEPS} events")
}

/// Poll `future` exactly once, so its operation is submitted, then drop it.
fn start_and_drop<F: Future>(future: F) {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(
        future.as_mut().poll(&mut context).is_pending(),
        "a storage operation completes through a scheduled event"
    );
}

/// Run every scheduled event.
fn drain(sim: &mut SimWorld) {
    for _ in 0..MAX_STEPS {
        if !sim.has_pending_events() {
            return;
        }
        sim.step();
    }
    panic!("the world did not settle within {MAX_STEPS} events");
}

fn open(sim: &mut SimWorld, path: &str, options: OpenOptions) -> SimStorageFile {
    let provider = sim.storage_provider(ip());
    settle(sim, provider.open(path, options)).expect("open")
}

/// A file holding `OLD`, synced.
fn file_with_old(sim: &mut SimWorld, path: &str) -> SimStorageFile {
    let file = open(sim, path, OpenOptions::read_write().create(true));
    let written = settle(sim, file.write_at(0, OLD)).expect("write old");
    assert_eq!(written, OLD.len());
    settle(sim, file.sync_all()).expect("sync old");
    file
}

/// What a fresh handle reads from `path`.
fn read_fresh(sim: &mut SimWorld, path: &str) -> Vec<u8> {
    let file = open(sim, path, OpenOptions::read_only());
    let size = settle(sim, file.size()).expect("size");
    let mut buf = vec![0; usize::try_from(size).expect("small file")];
    let read = settle(sim, file.read_at(0, &mut buf)).expect("read");
    buf.truncate(read);
    buf
}

#[test]
fn a_dropped_write_lands_and_a_fresh_read_sees_it() {
    let mut sim = world(1, 0.0);
    let file = file_with_old(&mut sim, "landed.bin");

    start_and_drop(file.write_at(0, NEW));
    assert!(
        sim.has_pending_events(),
        "a dropped write keeps its scheduled completion"
    );
    drain(&mut sim);

    assert_eq!(read_fresh(&mut sim, "landed.bin"), NEW);
    let counters = sim.disk_counters(ip());
    assert_eq!(counters.in_flight, 0, "the landed write left the disk");
    assert_eq!(counters.writes, 2, "the dropped write is counted as done");
}

#[test]
fn a_discarded_write_leaves_no_trace() {
    let mut sim = world(1, 1.0);
    let file = file_with_old(&mut sim, "discarded.bin");

    start_and_drop(file.write_at(0, NEW));
    assert!(
        !sim.has_pending_events(),
        "a discarded write's completion is unscheduled"
    );
    assert_eq!(sim.disk_counters(ip()).in_flight, 0);
    assert_eq!(read_fresh(&mut sim, "discarded.bin"), OLD);
}

#[test]
fn the_seed_decides_whether_a_dropped_write_lands_and_it_is_whole() {
    let outcome = |seed: u64| {
        let mut sim = world(seed, 0.5);
        let file = file_with_old(&mut sim, "coin.bin");
        start_and_drop(file.write_at(0, NEW));
        drain(&mut sim);
        let bytes = read_fresh(&mut sim, "coin.bin");
        assert!(
            bytes == OLD || bytes == NEW,
            "seed {seed}: a dropped write is discarded or lands whole, read {bytes:?}"
        );
        bytes == NEW
    };
    let outcomes: Vec<bool> = (0..64).map(outcome).collect();
    assert!(outcomes.iter().any(|landed| *landed), "no seed landed it");
    assert!(
        outcomes.iter().any(|landed| !landed),
        "no seed discarded it"
    );
    for seed in 0..8 {
        assert_eq!(
            outcome(seed),
            outcomes[usize::try_from(seed).expect("small seed")],
            "seed {seed} must replay"
        );
    }
}

#[test]
fn a_dropped_sync_still_makes_the_write_it_covers_durable() {
    let mut sim = world(7, 0.0);
    let file = file_with_old(&mut sim, "synced.bin");
    settle(&mut sim, file.write_at(0, NEW)).expect("write new");

    start_and_drop(file.sync_all());
    drain(&mut sim);
    drop(file);

    assert_eq!(
        sim.process_file_durable_bytes(ip(), "synced.bin")
            .expect("durable image"),
        NEW,
        "the dropped sync landed and committed the write"
    );
    sim.simulate_crash_for_process(ip(), true);
    assert_eq!(read_fresh(&mut sim, "synced.bin"), NEW);
}

#[test]
fn a_crash_before_a_dropped_write_lands_fails_it() {
    let mut sim = world(3, 0.0);
    let file = file_with_old(&mut sim, "crashed.bin");

    start_and_drop(file.write_at(0, NEW));
    assert!(sim.has_pending_events());
    sim.simulate_crash_for_process(ip(), true);
    assert!(
        !sim.has_pending_events(),
        "the crash unscheduled the dropped write"
    );
    drop(file);

    assert_eq!(read_fresh(&mut sim, "crashed.bin"), OLD);
    assert_eq!(sim.disk_counters(ip()).in_flight, 0);
}

#[test]
fn a_dropped_write_that_landed_unsynced_follows_the_crash_model() {
    for seed in 0..32 {
        let mut sim = world(seed, 0.0);
        let file = file_with_old(&mut sim, "unsynced.bin");
        start_and_drop(file.write_at(0, NEW));
        drain(&mut sim);
        drop(file);
        sim.simulate_crash_for_process(ip(), true);
        let bytes = read_fresh(&mut sim, "unsynced.bin");
        // Fault-free crash damage: each sector keeps old or new.
        let sector_ok = bytes.len() == OLD.len()
            && bytes
                .iter()
                .zip(OLD.iter().zip(NEW))
                .all(|(byte, (old, new))| byte == old || byte == new);
        assert!(
            sector_ok,
            "seed {seed}: impossible crash resolution {bytes:?}"
        );
    }
}

#[test]
fn a_wipe_before_a_dropped_write_lands_fails_it() {
    let mut sim = world(3, 0.0);
    let file = file_with_old(&mut sim, "wiped.bin");
    start_and_drop(file.write_at(0, NEW));
    sim.wipe_storage_for_process(ip());
    assert!(!sim.has_pending_events());
    drop(file);
    let provider = sim.storage_provider(ip());
    assert!(!settle(&mut sim, provider.exists("wiped.bin")).expect("exists"));
}

#[test]
fn closing_a_handle_lets_its_in_flight_stream_write_land() {
    let mut sim = world(5, 0.0);
    let mut file = open(&mut sim, "stream.bin", OpenOptions::create_write());
    start_and_drop(file.write(NEW));
    drop(file);
    assert!(
        sim.has_pending_events(),
        "closing the handle does not cancel the write"
    );
    drain(&mut sim);
    assert_eq!(read_fresh(&mut sim, "stream.bin"), NEW);
}

#[test]
fn an_unlinked_file_stays_reachable_until_a_dropped_write_on_it_lands() {
    let mut sim = world(5, 0.0);
    let file = file_with_old(&mut sim, "unlinked.bin");
    let provider = sim.storage_provider(ip());
    start_and_drop(file.write_at(0, NEW));
    drop(file);
    settle(&mut sim, provider.delete("unlinked.bin")).expect("delete");
    drain(&mut sim);
    assert_eq!(sim.disk_counters(ip()).in_flight, 0);
}

#[test]
fn a_dropped_set_len_lands() {
    let mut sim = world(9, 0.0);
    let file = file_with_old(&mut sim, "truncated.bin");
    start_and_drop(file.set_len(4));
    drain(&mut sim);
    assert_eq!(read_fresh(&mut sim, "truncated.bin"), &OLD[..4]);
}

#[test]
fn a_dropped_read_is_cancelled() {
    let mut sim = world(11, 0.0);
    let file = file_with_old(&mut sim, "read.bin");
    let mut buf = [0; 4];
    start_and_drop(file.read_at(0, &mut buf));
    assert!(
        !sim.has_pending_events(),
        "a dropped read has no effect to land"
    );
    assert_eq!(sim.disk_counters(ip()).in_flight, 0);
}

#[test]
fn a_dropped_mutation_parked_on_a_failed_disk_never_lands() {
    let mut sim = world(13, 0.0);
    let file = file_with_old(&mut sim, "parked.bin");
    sim.fail_disk_for_process(ip());
    start_and_drop(file.write_at(0, NEW));
    assert!(
        !sim.has_pending_events(),
        "a parked write has no completion"
    );
    assert_eq!(
        sim.disk_counters(ip()).in_flight,
        0,
        "dropping a parked write removes it"
    );
    drop(file);
    sim.simulate_crash_for_process(ip(), true);
    assert_eq!(read_fresh(&mut sim, "parked.bin"), OLD);
}

#[test]
fn shutdown_with_a_dropped_write_still_scheduled_is_quiet() {
    let mut sim = world(17, 0.0);
    let file = file_with_old(&mut sim, "shutdown.bin");
    start_and_drop(file.write_at(0, NEW));
    sim.schedule_event(Event::Shutdown, Duration::ZERO);
    sim.step();
    assert_eq!(sim.pending_event_count(), 0, "shutdown unscheduled it");
    assert_eq!(sim.disk_counters(ip()).in_flight, 0);
    drop(file);
}
