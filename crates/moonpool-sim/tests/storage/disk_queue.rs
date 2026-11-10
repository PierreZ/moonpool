//! Namespace operations are disk operations, and the disk has a queue.
//!
//! `open`, `delete`, `rename`, `create_dir_all` and `sync_dir` wait in the
//! disk queue and take effect when they complete, so a crash can land
//! between a `rename` and its completion and a failed disk hangs them. The
//! disk serves one operation at a time: parallel writes finish one after the
//! other, and a sync pays for the bytes it flushes.

use std::{
    future::Future,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::task::noop_waker;
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{LatencyDistribution, SimWorld, StorageConfiguration, storage::SimStorageFile};

const MAX_STEPS: usize = 100_000;

fn owner() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn fixed(latency: Duration) -> LatencyDistribution {
    LatencyDistribution::Uniform {
        start: latency,
        end: latency,
    }
}

/// A disk with no device latency and a free queue, except that namespace
/// operations take `namespace`.
fn namespace_disk(namespace: Duration) -> StorageConfiguration {
    StorageConfiguration {
        iops: u64::MAX,
        bandwidth: u64::MAX,
        read_latency: fixed(Duration::ZERO),
        write_latency: fixed(Duration::ZERO),
        sync_latency: fixed(Duration::ZERO),
        namespace_latency: fixed(namespace),
        ..StorageConfiguration::fast_local()
    }
}

/// Poll `future` and step the world until it is ready or nothing is left
/// to run.
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
    Poll::Pending
}

fn ready<T>(poll: Poll<T>, what: &str) -> T {
    match poll {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("{what} never completed"),
    }
}

/// Run `future` to completion and return its output and the time it took.
fn timed<F: Future>(sim: &mut SimWorld, future: F, what: &str) -> (F::Output, Duration) {
    let start = sim.now();
    let output = ready(settle(sim, future), what);
    let took = sim.now().checked_sub(start).expect("time moves forward");
    (output, took)
}

fn create(sim: &mut SimWorld, path: &str) -> SimStorageFile {
    let provider = sim.storage_provider(owner());
    ready(
        settle(sim, provider.open(path, OpenOptions::create_write())),
        "open",
    )
    .expect("open")
}

#[test]
fn namespace_operations_take_their_configured_latency() {
    let five_ms = Duration::from_millis(5);
    let mut sim = SimWorld::new();
    sim.set_storage_config(namespace_disk(five_ms));
    let provider = sim.storage_provider(owner());

    let (opened, took) = timed(
        &mut sim,
        provider.open("a", OpenOptions::create_write()),
        "open",
    );
    opened.expect("open");
    assert!(took >= five_ms, "open took {took:?}");

    let (exists, took) = timed(&mut sim, provider.exists("a"), "exists");
    assert!(exists.expect("exists"));
    assert_eq!(took, Duration::ZERO, "exists reads the cached namespace");

    let (result, took) = timed(&mut sim, provider.create_dir_all("d/e"), "mkdir");
    result.expect("create_dir_all");
    assert!(took >= five_ms, "create_dir_all took {took:?}");

    let (result, took) = timed(&mut sim, provider.rename("a", "d/b"), "rename");
    result.expect("rename");
    assert!(took >= five_ms, "rename took {took:?}");

    let (result, took) = timed(&mut sim, provider.sync_dir("d"), "sync_dir");
    result.expect("sync_dir");
    assert!(took >= five_ms, "sync_dir took {took:?}");

    let (result, took) = timed(&mut sim, provider.delete("d/b"), "delete");
    result.expect("delete");
    assert!(took >= five_ms, "delete took {took:?}");
}

#[test]
fn a_crash_before_a_rename_completes_leaves_the_old_name() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(namespace_disk(Duration::from_millis(5)));
    drop(create(&mut sim, "old"));
    let provider = sim.storage_provider(owner());

    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    let mut rename = pin!(provider.rename("old", "new"));
    assert!(rename.as_mut().poll(&mut context).is_pending());
    assert!(
        ready(settle(&mut sim, provider.exists("old")), "exists").expect("exists"),
        "a rename in flight has not taken effect"
    );

    sim.simulate_crash_for_process(owner(), true);
    let Poll::Ready(result) = rename.as_mut().poll(&mut context) else {
        panic!("the crash fails the rename in flight");
    };
    let error = result.expect_err("the rename was interrupted");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(ready(settle(&mut sim, provider.exists("old")), "exists").expect("exists"));
    assert!(!ready(settle(&mut sim, provider.exists("new")), "exists").expect("exists"));
}

#[test]
fn a_failed_disk_hangs_namespace_operations() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(namespace_disk(Duration::from_millis(5)));
    drop(create(&mut sim, "a"));
    sim.fail_disk_for_process(owner());
    let provider = sim.storage_provider(owner());
    assert!(
        settle(&mut sim, provider.sync_dir(".")).is_pending(),
        "sync_dir on a failed disk never completes"
    );
    assert!(
        settle(&mut sim, provider.open("b", OpenOptions::create_write())).is_pending(),
        "open on a failed disk never completes"
    );
}

#[test]
fn a_dropped_rename_still_lands() {
    let mut sim = SimWorld::new();
    sim.set_storage_config(namespace_disk(Duration::from_millis(5)));
    drop(create(&mut sim, "old"));
    let provider = sim.storage_provider(owner());
    {
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut rename = pin!(provider.rename("old", "new"));
        assert!(rename.as_mut().poll(&mut context).is_pending());
    }
    let idle = sim.sleep(Duration::from_millis(10));
    ready(settle(&mut sim, idle), "sleep").expect("sleep");
    assert!(ready(settle(&mut sim, provider.exists("new")), "exists").expect("exists"));
    assert!(!ready(settle(&mut sim, provider.exists("old")), "exists").expect("exists"));
}

#[test]
fn parallel_writes_wait_for_each_other_in_the_disk_queue() {
    // One I/O per millisecond, and no other latency.
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration {
        iops: 1_000,
        ..namespace_disk(Duration::ZERO)
    });
    let files: Vec<SimStorageFile> = (0..10)
        .map(|at| create(&mut sim, &at.to_string()))
        .collect();
    let writes = futures::future::join_all(files.iter().map(|file| file.write_at(0, b"x")));
    let (results, took) = timed(&mut sim, writes, "writes");
    for result in results {
        result.expect("write");
    }
    assert!(
        took >= Duration::from_millis(10),
        "ten writes are served one after the other, took {took:?}"
    );
}

#[test]
fn a_sync_pays_for_the_bytes_it_flushes() {
    // One megabyte per second, and no other latency.
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration {
        bandwidth: 1_000_000,
        ..namespace_disk(Duration::ZERO)
    });
    let file = create(&mut sim, "data");
    let (result, clean) = timed(&mut sim, file.sync_all(), "clean sync");
    result.expect("sync");

    ready(
        settle(&mut sim, file.write_at(0, &vec![7; 100_000])),
        "write",
    )
    .expect("write");
    let (result, dirty) = timed(&mut sim, file.sync_all(), "dirty sync");
    result.expect("sync");
    assert!(
        clean < Duration::from_millis(1),
        "a clean sync took {clean:?}"
    );
    assert!(
        dirty >= Duration::from_millis(100),
        "a sync of 100 KB at 1 MB/s took {dirty:?}"
    );
}
