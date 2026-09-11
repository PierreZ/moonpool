//! One namespace per process: a path names a file *on a process's disk*.
//!
//! Providers for distinct process IPs used to resolve the same relative path
//! to the same file, and a directory sync from one node committed another
//! node's directory entries. Independent replicas could overwrite each
//! other, share bytes, or make another disk's metadata durable — so
//! replication and crash tests did not model independent storage.

use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};
use std::net::IpAddr;

fn node_a() -> IpAddr {
    "10.0.1.1".parse().expect("valid IP")
}

fn node_b() -> IpAddr {
    "10.0.1.2".parse().expect("valid IP")
}

fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

/// A world whose crashes always lose an unsynced directory entry.
fn losing_sim() -> SimWorld {
    let mut config = StorageConfiguration::fast_local();
    config.unsynced_dir_entry_loss_probability = 1.0;
    let mut sim = SimWorld::new();
    sim.set_storage_config(config);
    sim
}

async fn run_as<F, Fut, T>(sim: &mut SimWorld, ip: IpAddr, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let provider = sim.storage_provider(ip);
    let handle = tokio::spawn(f(provider));
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

async fn write_file(
    provider: &SimStorageProvider,
    path: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut file = provider.open(path, OpenOptions::create_new_write()).await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

async fn read_file(provider: &SimStorageProvider, path: &str) -> std::io::Result<Vec<u8>> {
    let mut file = provider.open(path, OpenOptions::read_only()).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// Two processes each create `data.db`: two files, each seeing its own
/// bytes, each unaffected by the other's delete.
#[test]
fn each_process_resolves_a_path_on_its_own_disk() {
    local_runtime().block_on(async {
        let mut sim = SimWorld::new();
        sim.set_storage_config(StorageConfiguration::fast_local());

        run_as(&mut sim, node_a(), |provider| async move {
            write_file(&provider, "data.db", b"from-a").await
        })
        .await
        .expect("node A creates its file");

        run_as(&mut sim, node_b(), |provider| async move {
            // `create_new` on B must not collide with A's file of the same name.
            write_file(&provider, "data.db", b"from-b").await
        })
        .await
        .expect("node B creates its own file under the same path");

        let a = run_as(&mut sim, node_a(), |provider| async move {
            read_file(&provider, "data.db").await
        })
        .await
        .expect("A reads");
        assert_eq!(a, b"from-a", "A's bytes are A's, whatever B wrote");

        run_as(&mut sim, node_b(), |provider| async move {
            provider.delete("data.db").await?;
            provider.exists("data.db").await
        })
        .await
        .map(|exists| assert!(!exists, "B's name is gone"))
        .expect("B deletes its file");

        let still_there = run_as(&mut sim, node_a(), |provider| async move {
            let exists = provider.exists("data.db").await?;
            let bytes = read_file(&provider, "data.db").await?;
            Ok::<_, std::io::Error>((exists, bytes))
        })
        .await
        .expect("A still reads");
        assert_eq!(
            still_there,
            (true, b"from-a".to_vec()),
            "B's delete must not reach A's disk"
        );
    });
}

/// A's directory sync commits A's entries only: B's unsynced entry is still
/// lost when B crashes.
#[test]
fn a_directory_sync_commits_only_the_syncing_processs_entries() {
    local_runtime().block_on(async {
        let mut sim = losing_sim();

        for ip in [node_a(), node_b()] {
            run_as(&mut sim, ip, |provider| async move {
                write_file(&provider, "db/wal", b"entries").await
            })
            .await
            .expect("each node creates its wal");
        }
        run_as(&mut sim, node_a(), |provider| async move {
            provider.sync_dir("db").await
        })
        .await
        .expect("A syncs its directory");

        sim.simulate_crash_for_process(node_b(), true);

        let b_exists = run_as(&mut sim, node_b(), |provider| async move {
            provider.exists("db/wal").await
        })
        .await
        .expect("exists");
        assert!(
            !b_exists,
            "B never synced its directory: A's sync must not have saved B's entry"
        );

        let a_exists = run_as(&mut sim, node_a(), |provider| async move {
            provider.exists("db/wal").await
        })
        .await
        .expect("exists");
        assert!(a_exists, "A's own entry is untouched by B's crash");
    });
}
