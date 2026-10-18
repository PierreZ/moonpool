//! A disk with a capacity fills up.
//!
//! A write past the end of a file, or a `set_len` that grows one, fails with
//! `ENOSPC` (`io::ErrorKind::StorageFull`) when its growth does not fit, and
//! none of its bytes land. A write inside a file's length needs no space.
//! Deleting or shrinking a file frees space at once. Recovery mode lifts
//! every capacity, and nothing here draws from the random stream.

use std::{
    future::Future,
    io::ErrorKind,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
};

use futures::task::noop_waker;
use moonpool_core::{DiskSpace, OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{DiskCapacity, SimFaultEvent, SimWorld, StorageConfiguration, rng_call_count};

/// How many events a driven future may consume before it is declared stuck.
const MAX_STEPS: usize = 10_000;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

fn settle<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    for _ in 0..MAX_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(sim.has_pending_events(), "a disk-full future parked");
        sim.step();
    }
    panic!("simulation-backed future exceeded {MAX_STEPS} events")
}

fn world() -> SimWorld {
    let mut sim = SimWorld::new_with_seed(20_261_018);
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

fn open(sim: &mut SimWorld, owner: IpAddr, path: &str) -> moonpool_sim::storage::SimStorageFile {
    let provider = sim.storage_provider(owner);
    settle(
        sim,
        provider.open(path, OpenOptions::create_write().read(true)),
    )
    .expect("open")
}

fn full_faults(sim: &SimWorld) -> usize {
    sim.take_faults()
        .iter()
        .filter(|record| {
            matches!(&record.event, SimFaultEvent::StorageWriteFault { write_kind, .. } if write_kind == "full")
        })
        .count()
}

#[test]
fn a_write_that_does_not_fit_fails_with_enospc_and_lands_nothing() {
    let mut sim = world();
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(1_000)));
    let file = open(&mut sim, ip(1), "data.bin");

    settle(&mut sim, file.write_at(0, &[7; 600])).expect("600 bytes fit");
    let refused = settle(&mut sim, file.write_at(600, &[8; 401])).expect_err("401 more do not");
    assert_eq!(refused.kind(), ErrorKind::StorageFull);
    assert_eq!(settle(&mut sim, file.size()).expect("size"), 600);
    assert_eq!(full_faults(&sim), 1, "the refusal is one fault event");
    assert_eq!(
        sim.disk_space(ip(1)),
        Some(DiskSpace {
            total_bytes: 1_000,
            free_bytes: 400
        })
    );

    // Exactly what is left still fits, and a rewrite inside the file needs
    // no space at all on the full disk.
    settle(&mut sim, file.write_at(600, &[8; 400])).expect("400 bytes fit");
    assert_eq!(sim.disk_space(ip(1)).map(|space| space.free_bytes), Some(0));
    settle(&mut sim, file.write_at(0, &[9; 1_000])).expect("a rewrite needs no space");
    let mut back = vec![0; 1_000];
    settle(&mut sim, file.read_at(0, &mut back)).expect("read");
    assert_eq!(back, vec![9; 1_000]);
}

#[test]
fn deleting_or_shrinking_a_file_frees_its_space() {
    let mut sim = world();
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(1_000)));
    let provider = sim.storage_provider(ip(1));
    let old = open(&mut sim, ip(1), "old.bin");
    let new = open(&mut sim, ip(1), "new.bin");
    settle(&mut sim, old.write_at(0, &[1; 900])).expect("fits");

    let refused = settle(&mut sim, new.set_len(200)).expect_err("a grown set_len does not fit");
    assert_eq!(refused.kind(), ErrorKind::StorageFull);
    assert_eq!(settle(&mut sim, new.size()).expect("size"), 0);

    settle(&mut sim, old.set_len(700)).expect("a shrink always fits");
    settle(&mut sim, new.set_len(200)).expect("the shrink freed 200 bytes");

    drop(old);
    settle(&mut sim, provider.delete("old.bin")).expect("delete");
    settle(&mut sim, new.write_at(200, &[2; 800])).expect("the delete freed the rest");
    assert_eq!(sim.disk_space(ip(1)).map(|space| space.free_bytes), Some(0));
}

#[test]
fn an_amplified_disk_fills_faster_and_other_disks_are_unaffected() {
    let mut sim = world();
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(1_000).amplified(4)));
    let small = open(&mut sim, ip(1), "data.bin");
    let roomy = open(&mut sim, ip(2), "data.bin");

    settle(&mut sim, small.write_at(0, &[1; 250])).expect("250 bytes count 1,000");
    let refused =
        settle(&mut sim, small.write_at(250, &[1])).expect_err("one more byte does not fit");
    assert_eq!(refused.kind(), ErrorKind::StorageFull);
    settle(&mut sim, roomy.write_at(0, &[1; 10_000])).expect("a disk with no capacity has room");
    assert_eq!(sim.disk_space(ip(2)), None);
}

#[test]
fn a_capacity_holds_across_a_crash() {
    let mut sim = world();
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(100)));
    let file = open(&mut sim, ip(1), "data.bin");
    settle(&mut sim, file.write_at(0, &[1; 100])).expect("fits");
    settle(&mut sim, file.sync_all()).expect("sync");
    drop(file);
    sim.simulate_crash_for_process(ip(1), true);

    let file = open(&mut sim, ip(1), "data.bin");
    let refused = settle(&mut sim, file.write_at(100, &[1])).expect_err("still full after a crash");
    assert_eq!(refused.kind(), ErrorKind::StorageFull);
}

#[test]
fn recovery_mode_lifts_every_capacity_and_refuses_new_ones() {
    let mut sim = world();
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(10)));
    let file = open(&mut sim, ip(1), "data.bin");
    assert_eq!(
        settle(&mut sim, file.write_at(0, &[1; 11]))
            .expect_err("full")
            .kind(),
        ErrorKind::StorageFull
    );

    sim.enter_recovery_mode();
    assert_eq!(sim.disk_capacity(ip(1)), None);
    settle(&mut sim, file.write_at(0, &[1; 11])).expect("the tail has room");
    sim.set_disk_capacity(ip(1), Some(DiskCapacity::new(10)));
    assert_eq!(
        sim.disk_capacity(ip(1)),
        None,
        "recovery mode ignores a new capacity"
    );
}

#[test]
fn a_capacity_draws_nothing_from_the_random_stream() {
    let draws = |capacity: Option<DiskCapacity>| {
        let mut sim = world();
        sim.set_disk_capacity(ip(1), capacity);
        let file = open(&mut sim, ip(1), "data.bin");
        let before = rng_call_count();
        settle(&mut sim, file.write_at(0, &[1; 64])).expect("fits");
        settle(&mut sim, file.sync_all()).expect("sync");
        rng_call_count() - before
    };
    assert_eq!(draws(None), draws(Some(DiskCapacity::new(1 << 20))));
}
