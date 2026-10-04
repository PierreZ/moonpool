//! Owner-scoped, byte-range targeted injection: one copy of one record on
//! one replica, and nothing of another process's file at the same path
//! (moonpool#296).

use std::net::IpAddr;

use crate::{local_runtime, run_as};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{EioTarget, SimWorld, StorageConfiguration, StorageError};

const LEN: usize = 2048;

fn node_a() -> IpAddr {
    "10.0.1.1".parse().expect("valid IP")
}

fn node_b() -> IpAddr {
    "10.0.1.2".parse().expect("valid IP")
}

fn pattern() -> Vec<u8> {
    (0..LEN).map(|at| at.to_le_bytes()[0] ^ 0x5A).collect()
}

/// Every replica writes and syncs the same `data`.
fn world() -> SimWorld {
    let mut sim = SimWorld::new_with_seed(5);
    sim.set_storage_config(StorageConfiguration {
        clean_crash_probability: 0.0,
        crash_lost_probability: 1.0,
        ..StorageConfiguration::fast_local()
    });
    for ip in [node_a(), node_b()] {
        local_runtime().block_on(async {
            run_as(&mut sim, ip, |provider| async move {
                let file = provider
                    .open("data", OpenOptions::create_write().read(true))
                    .await?;
                file.write_at(0, &pattern()).await?;
                file.sync_all().await
            })
            .await
            .expect("write");
        });
    }
    sim
}

fn read(sim: &mut SimWorld, ip: IpAddr) -> std::io::Result<Vec<u8>> {
    local_runtime().block_on(async {
        run_as(sim, ip, |provider| async move {
            let file = provider.open("data", OpenOptions::read_only()).await?;
            let mut bytes = vec![0; LEN];
            file.read_at(0, &mut bytes).await?;
            Ok(bytes)
        })
        .await
    })
}

fn differing(bytes: &[u8]) -> Vec<usize> {
    let clean = pattern();
    (0..LEN).filter(|at| bytes[*at] != clean[*at]).collect()
}

#[test]
fn corrupt_bytes_changes_exactly_those_bytes_of_one_owner() {
    let mut sim = world();
    // Straddles the boundary between sectors 0 and 1.
    sim.corrupt_process_file_bytes(node_a(), "data", 510..514)
        .expect("corrupt");
    assert_eq!(
        differing(&read(&mut sim, node_a()).expect("read a")),
        vec![510, 511, 512, 513]
    );
    assert!(
        differing(&read(&mut sim, node_b()).expect("read b")).is_empty(),
        "the other owner reads clean"
    );

    // The change is durable, and the oracle does not take it for a lost
    // synced write.
    sim.simulate_crash_for_process(node_a(), true);
    assert_eq!(
        differing(&read(&mut sim, node_a()).expect("reread a")),
        vec![510, 511, 512, 513]
    );
}

#[test]
fn corrupt_bytes_is_a_pure_function_of_the_range() {
    let run = || {
        let mut sim = world();
        sim.corrupt_process_file_bytes(node_b(), "data", 7..40)
            .expect("corrupt");
        read(&mut sim, node_b()).expect("read")
    };
    assert_eq!(run(), run());
}

#[test]
fn corrupt_bytes_refuses_a_range_past_the_durable_end() {
    let sim = world();
    let end = u64::try_from(LEN).expect("small");
    assert!(matches!(
        sim.corrupt_process_file_bytes(node_a(), "data", end - 1..end + 1),
        Err(StorageError::OutOfRange { .. })
    ));
    assert!(matches!(
        sim.corrupt_process_file_bytes(node_a(), "missing", 0..1),
        Err(StorageError::NotFound { .. })
    ));
}

#[test]
fn an_owner_scoped_eio_fails_only_that_owner() {
    let mut sim = world();
    sim.fail_process_file_with_eio(node_a(), "data", 1..2, EioTarget::Read)
        .expect("arm");
    assert!(read(&mut sim, node_a()).is_err(), "A's read fails");
    assert!(read(&mut sim, node_b()).is_ok(), "B's read is untouched");
    sim.clear_process_file_eio(node_a(), "data", EioTarget::Read)
        .expect("clear");
    assert!(read(&mut sim, node_a()).is_ok(), "cleared");
}
