//! The simulated disk's counters, as `/proc/diskstats` would show them.
//!
//! `io_ticks` grows while at least one operation is in flight, never past the
//! sample time; reads, writes, bytes and times grow on completed transfers.
//! The same numbers reach a process through `SimSystemProvider`.

use std::{
    future::Future,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{io::AsyncWriteExt, task::noop_waker};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider, SystemProvider};
use moonpool_sim::{LatencyDistribution, SimSystemProvider, SimWorld, StorageConfiguration};

const MAX_STEPS: usize = 10_000;

fn ip(last_octet: u8) -> IpAddr {
    format!("10.0.1.{last_octet}")
        .parse()
        .expect("valid test IP")
}

fn fixed(latency: Duration) -> LatencyDistribution {
    LatencyDistribution::Uniform {
        start: latency,
        end: latency,
    }
}

/// A disk whose every operation takes exactly 10 ms (plus the IOPS and
/// bandwidth terms, which this configuration makes zero).
fn ten_ms_disk() -> StorageConfiguration {
    let ten_ms = Duration::from_millis(10);
    StorageConfiguration {
        iops: u64::MAX,
        bandwidth: u64::MAX,
        read_latency: fixed(ten_ms),
        write_latency: fixed(ten_ms),
        sync_latency: fixed(ten_ms),
        ..StorageConfiguration::fast_local()
    }
}

fn settle<F: Future>(sim: &mut SimWorld, future: F) -> Poll<F::Output> {
    let mut future = pin!(future);
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

#[test]
fn sequential_io_is_busy_the_whole_time() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(ten_ms_disk());
    let owner = ip(1);
    let mut file = open(&mut sim, owner, "data");
    let start = sim.now();
    ready(settle(&mut sim, file.write_all(b"four")), "write").expect("write");
    ready(settle(&mut sim, file.sync_all()), "sync").expect("sync");
    let elapsed = sim.now().checked_sub(start).expect("time moves forward");
    // Two 10 ms operations, plus the nanosecond of the IOPS term each.
    assert!(elapsed >= Duration::from_millis(20) && elapsed < Duration::from_millis(21));

    let disk = sim.disk_counters(owner);
    assert_eq!(disk.io_ticks, elapsed);
    assert_eq!(disk.in_flight, 0);
    assert_eq!(disk.writes, 1);
    assert_eq!(disk.write_bytes, 4);
    assert!(disk.write_time >= Duration::from_millis(10) && disk.write_time < elapsed);
    assert_eq!(disk.reads, 0);
    assert_eq!(disk.device, "simdisk");

    // Another process's disk saw nothing.
    assert_eq!(sim.disk_counters(ip(2)).io_ticks, Duration::ZERO);
}

#[test]
fn overlapping_io_counts_wall_time_once() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(ten_ms_disk());
    let owner = ip(1);
    let mut a = open(&mut sim, owner, "a");
    let mut b = open(&mut sim, owner, "b");
    let start = sim.now();
    let both = futures::future::join(a.write_all(b"one"), b.write_all(b"two"));
    let (first, second) = ready(settle(&mut sim, both), "writes");
    first.expect("write a");
    second.expect("write b");
    let elapsed = sim.now().checked_sub(start).expect("time moves forward");
    assert!(elapsed >= Duration::from_millis(10) && elapsed < Duration::from_millis(11));

    let disk = sim.disk_counters(owner);
    assert_eq!(disk.io_ticks, elapsed, "overlapping time counts once");
    assert_eq!(disk.writes, 2);
    assert_eq!(
        disk.write_time,
        elapsed * 2,
        "each write counts its own time"
    );
}

#[test]
fn an_operation_in_flight_counts_up_to_the_sample() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(ten_ms_disk());
    let owner = ip(1);
    let mut file = open(&mut sim, owner, "data");
    sim.fail_disk_for_process(owner);
    let start = sim.now();
    let mut write = pin!(file.write_all(b"hung"));
    let waker = noop_waker();
    assert!(
        write
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(sim.disk_counters(owner).in_flight, 1);
    assert_eq!(sim.disk_counters(owner).io_ticks, Duration::ZERO);

    let sleep = sim.sleep(Duration::from_secs(3));
    ready(settle(&mut sim, sleep), "sleep").expect("sleep");
    let disk = sim.disk_counters(owner);
    assert_eq!(
        sim.now().checked_sub(start).expect("time moves forward"),
        Duration::from_secs(3)
    );
    assert_eq!(disk.in_flight, 1);
    assert_eq!(disk.io_ticks, Duration::from_secs(3));
    assert_eq!(disk.writes, 0, "a hung write never completes");

    // The reboot fails the hung write: the disk is idle again, and the
    // counters never go back.
    sim.simulate_crash_for_process(owner, true);
    let after = sim.disk_counters(owner);
    assert_eq!(after.in_flight, 0);
    assert_eq!(after.io_ticks, Duration::from_secs(3));
    let sleep = sim.sleep(Duration::from_secs(1));
    ready(settle(&mut sim, sleep), "sleep").expect("sleep");
    assert_eq!(sim.disk_counters(owner).io_ticks, Duration::from_secs(3));
}

#[test]
fn the_system_provider_reports_the_disk_counters() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(ten_ms_disk());
    let owner = ip(1);
    let mut file = open(&mut sim, owner, "data");
    ready(settle(&mut sim, file.write_all(b"four")), "write").expect("write");

    let provider = SimSystemProvider::new(sim.downgrade(), owner);
    let sample = provider.sample("ignored").expect("sample");
    assert_eq!(sample.at, sim.now());
    assert_eq!(sample.process_cpu, Duration::ZERO);
    assert_eq!(sample.cores, 1);
    assert_eq!(sample.disk, Some(sim.disk_counters(owner)));
}

#[test]
fn the_world_remembers_the_samples_it_handed_out() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(ten_ms_disk());
    let owner = ip(1);
    let provider = SimSystemProvider::new(sim.downgrade(), owner);
    let first = provider.sample("").expect("sample");
    let mut file = open(&mut sim, owner, "data");
    ready(settle(&mut sim, file.write_all(b"four")), "write").expect("write");
    let second = provider.sample("").expect("sample");
    assert_eq!(sim.system_samples(owner), vec![first, second]);
    assert!(sim.system_samples(ip(2)).is_empty());

    for _ in 0..moonpool_sim::SYSTEM_SAMPLES_KEPT {
        provider.sample("").expect("sample");
    }
    assert_eq!(
        sim.system_samples(owner).len(),
        moonpool_sim::SYSTEM_SAMPLES_KEPT
    );
}
