//! A failed disk answers nothing.
//!
//! Every read, write, sync, or `set_len` issued to a failed disk is accepted
//! and stays `Pending` for the rest of the run. This is `FoundationDB`'s
//! `failedDisk` (`waitUntilDiskReady()` returns `Never()`), and it is
//! deliberately not a stall: a stall has an expiry and the caller's I/O
//! completes late, a failure has none and only the caller's own timeout, or
//! its process being killed, unblocks it. The floor is a count, not a rate:
//! at most one disk fails at a time, and a crash or wipe of the owning process
//! replaces it.

use std::{
    future::Future,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
};

use futures::{io::AsyncWriteExt, task::noop_waker};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimFaultEvent, SimWorld, StorageConfiguration, rng_call_count};

/// How many events a driven future may consume before it is declared stuck.
const MAX_STEPS: usize = 10_000;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

fn failing_disk() -> StorageConfiguration {
    StorageConfiguration {
        disk_failure_probability: 1.0,
        ..StorageConfiguration::fast_local()
    }
}

/// Poll `future` until it resolves or the world runs out of events.
///
/// A `Pending` answer means the future is parked with nothing left that could
/// ever wake it: the observable shape of an I/O that never completes.
fn settle<F: Future>(sim: &mut SimWorld, future: F) -> Poll<F::Output> {
    let mut future = pin!(future);
    settle_pinned(sim, future.as_mut())
}

fn settle_pinned<F: Future + ?Sized>(
    sim: &mut SimWorld,
    mut future: std::pin::Pin<&mut F>,
) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    for _ in 0..MAX_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Poll::Ready(output);
        }
        if !sim.has_pending_events() {
            return Poll::Pending;
        }
        sim.step();
    }
    panic!("simulation-backed future exceeded {MAX_STEPS} events")
}

fn ready<T>(poll: Poll<T>, what: &str) -> T {
    match poll {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("{what} parked with an empty world"),
    }
}

fn open(sim: &mut SimWorld, owner: IpAddr, path: &str) -> moonpool_sim::storage::SimStorageFile {
    let provider = sim.storage_provider(owner);
    ready(
        settle(sim, provider.open(path, OpenOptions::create_write())),
        "open",
    )
    .expect("open")
}

/// A write or sync completes on a healthy disk; used as the control.
fn write_and_sync(
    sim: &mut SimWorld,
    file: &mut moonpool_sim::storage::SimStorageFile,
) -> Poll<()> {
    settle(sim, async {
        file.write_all(b"payload").await.expect("write");
        file.sync_all().await.expect("sync");
    })
}

#[test]
fn a_failed_disk_parks_every_later_operation() {
    let mut sim = SimWorld::new_with_seed(20_260_904);
    sim.set_storage_config(failing_disk());
    let mut file = open(&mut sim, ip(1), "hung.bin");

    // The first I/O draws the coin, fails the disk, and is itself parked.
    assert!(
        settle(&mut sim, file.write_all(b"never")).is_pending(),
        "the write that failed the disk must never complete"
    );
    assert!(sim.is_disk_failed(ip(1)));
    assert_eq!(
        sim.pending_event_count(),
        0,
        "a parked operation owns no scheduled completion"
    );
    let faults = sim.take_faults();
    assert!(
        faults
            .iter()
            .any(|record| matches!(&record.event, SimFaultEvent::StorageDiskFailure { ip } if ip == "10.0.1.1")),
        "the failure is recorded once as a fault event, got {faults:?}"
    );

    // Everything after it parks too, and no second fault event is recorded.
    assert!(
        settle(&mut sim, file.sync_all()).is_pending(),
        "a sync on a failed disk must never complete"
    );
    assert!(
        settle(&mut sim, file.set_len(0)).is_pending(),
        "a set_len on a failed disk must never complete"
    );
    assert!(sim.take_faults().is_empty());
}

#[test]
fn the_coin_fails_one_disk_at_a_time() {
    let mut sim = SimWorld::new_with_seed(20_260_904);
    sim.set_storage_config(failing_disk());
    let mut first = open(&mut sim, ip(1), "first.bin");
    let mut second = open(&mut sim, ip(2), "second.bin");

    assert!(settle(&mut sim, first.write_all(b"never")).is_pending());
    assert!(sim.is_disk_failed(ip(1)));

    // Disk 2 would fail on its first operation too, but the budget is spent.
    assert!(
        write_and_sync(&mut sim, &mut second).is_ready(),
        "a second disk must keep working while one is already failed"
    );
    assert!(!sim.is_disk_failed(ip(2)));
}

#[test]
fn a_crash_replaces_the_disk_and_frees_the_budget() {
    let mut sim = SimWorld::new_with_seed(20_260_904);
    sim.set_storage_config(failing_disk());
    let mut first = open(&mut sim, ip(1), "first.bin");
    let mut second = open(&mut sim, ip(2), "second.bin");

    let mut parked = pin!(first.write_all(b"never"));
    assert!(settle_pinned(&mut sim, parked.as_mut()).is_pending());
    assert!(sim.is_disk_failed(ip(1)));
    assert!(write_and_sync(&mut sim, &mut second).is_ready());

    // The reboot is the disk replacement: the parked write is failed with the
    // rest of the process's in-flight I/O, and the failure is gone.
    sim.simulate_crash_for_process(ip(1), true);
    assert!(
        matches!(
            settle_pinned(&mut sim, parked.as_mut()),
            Poll::Ready(Err(_))
        ),
        "a crash must fail the operations the disk parked"
    );
    assert!(!sim.is_disk_failed(ip(1)));

    // The budget is free again, so the next disk to draw the coin fails.
    assert!(
        settle(&mut sim, second.write_all(b"never")).is_pending(),
        "once the first failure is cleared another disk may fail"
    );
    assert!(sim.is_disk_failed(ip(2)));
}

#[test]
fn a_scripted_failure_parks_without_drawing_randomness() {
    let mut sim = SimWorld::new_with_seed(20_260_904);
    sim.set_storage_config(StorageConfiguration::fast_local());
    let mut file = open(&mut sim, ip(1), "scripted.bin");
    assert!(write_and_sync(&mut sim, &mut file).is_ready());

    sim.fail_disk_for_process(ip(1));
    assert!(sim.is_disk_failed(ip(1)));
    let draws_before = rng_call_count();
    assert!(
        settle(&mut sim, file.sync_all()).is_pending(),
        "a scripted failure parks the next operation"
    );
    assert_eq!(
        rng_call_count(),
        draws_before,
        "a parked operation samples no latency and enters no episode"
    );
    assert!(
        sim.take_faults()
            .iter()
            .any(|record| matches!(record.event, SimFaultEvent::StorageDiskFailure { .. }))
    );
}
