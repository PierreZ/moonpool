//! Failed writeback (fsyncgate): a sync error marks the dirty sectors clean
//! without making them durable, and only some handles see the error.

use crate::{fast_sim, local_runtime, step_until_done, test_ip};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration, WritebackErrorReporting};

const PATH: &str = "fsyncgate.dat";

/// A sim whose file holds durable `old!`, with every later write's
/// writeback failing.
async fn sim_with_durable_old(revert: f64, reporting: WritebackErrorReporting) -> SimWorld {
    let mut sim = fast_sim();
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(async move {
        let file = provider.open(PATH, OpenOptions::create_write()).await?;
        file.write_at(0, b"old!").await?;
        file.sync_all().await
    });
    step_until_done(&mut sim, handle)
        .await
        .expect("durable write");

    let mut config = StorageConfiguration::fast_local();
    config.writeback_error_probability = 1.0;
    config.writeback_error_revert_probability = revert;
    config.writeback_error_reporting = reporting;
    // A clean crash lands every flushable write: only the stranded one is lost.
    config.clean_crash_probability = 1.0;
    sim.set_process_storage_config(test_ip(), config);
    sim
}

async fn read_file(sim: &mut SimWorld) -> Vec<u8> {
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(async move {
        let file = provider.open(PATH, OpenOptions::read_only()).await?;
        let mut buf = [0u8; 4];
        file.read_at(0, &mut buf).await?;
        Ok::<_, std::io::Error>(buf.to_vec())
    });
    step_until_done(sim, handle).await.expect("read")
}

/// The retried sync succeeds, reads still return the new bytes, and a crash
/// loses them: the trap PostgreSQL fell into.
#[test]
fn retried_sync_succeeds_and_crash_loses_the_write() {
    local_runtime().block_on(async {
        let mut sim = sim_with_durable_old(0.0, WritebackErrorReporting::Errseq).await;
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let file = provider.open(PATH, OpenOptions::read_write()).await?;
            file.write_at(0, b"new!").await?;
            let first = file.sync_all().await;
            let retry = file.sync_all().await;
            Ok::<_, std::io::Error>((first.is_err(), retry.is_ok()))
        });
        let (first_failed, retry_ok) = step_until_done(&mut sim, handle).await.expect("io");
        assert!(first_failed, "the sync after a failed writeback reports it");
        assert!(retry_ok, "the retried sync reports success");
        assert_eq!(
            read_file(&mut sim).await,
            b"new!",
            "the cache keeps the new bytes"
        );

        sim.simulate_crash_for_process(test_ip(), true);
        assert_eq!(
            read_file(&mut sim).await,
            b"old!",
            "the write a retried sync called durable is lost"
        );
    });
}

/// The revert variant drops the new bytes from the cache at once.
#[test]
fn reverted_writeback_drops_the_new_bytes() {
    local_runtime().block_on(async {
        let mut sim = sim_with_durable_old(1.0, WritebackErrorReporting::Errseq).await;
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let file = provider.open(PATH, OpenOptions::read_write()).await?;
            file.write_at(0, b"new!").await?;
            Ok::<_, std::io::Error>(file.sync_all().await.is_err())
        });
        assert!(step_until_done(&mut sim, handle).await.expect("io"));
        assert_eq!(read_file(&mut sim).await, b"old!");
    });
}

/// A write that dirties a stranded sector again makes it flushable again.
#[test]
fn rewrite_after_failed_writeback_is_durable() {
    local_runtime().block_on(async {
        let mut sim = sim_with_durable_old(0.0, WritebackErrorReporting::Errseq).await;
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let file = provider.open(PATH, OpenOptions::read_write()).await?;
            file.write_at(0, b"new!").await?;
            let _ = file.sync_all().await;
            Ok::<_, std::io::Error>(file)
        });
        let file = step_until_done(&mut sim, handle).await.expect("io");
        let mut config = StorageConfiguration::fast_local();
        config.clean_crash_probability = 0.0;
        sim.set_process_storage_config(test_ip(), config);
        let handle = tokio::spawn(async move {
            file.write_at(0, b"redo").await?;
            file.sync_all().await
        });
        step_until_done(&mut sim, handle)
            .await
            .expect("rewrite is durable");
        sim.simulate_crash_for_process(test_ip(), true);
        assert_eq!(read_file(&mut sim).await, b"redo");
    });
}

/// Which handles see the error, per kernel variant. `before` is open when
/// the writeback fails and wrote nothing; `writer` caused it; `after`
/// opens later. They sync in that order: `after`, `before`, `writer`, then
/// a handle opened at the end.
#[test]
fn reporting_variants_pick_the_handles_that_see_the_error() {
    use WritebackErrorReporting::{Errseq, FirstCaller, OpenedBefore, Silent};
    // (variant, after, before, writer, last): whether each sync fails.
    let cases = [
        (Errseq, true, true, true, false),
        (OpenedBefore, false, true, true, false),
        (FirstCaller, true, false, false, false),
        (Silent, false, false, false, false),
    ];
    for (reporting, after, before, writer, last) in cases {
        local_runtime().block_on(async {
            let mut sim = sim_with_durable_old(0.0, reporting).await;
            let provider = sim.storage_provider(test_ip());
            let handle = tokio::spawn(async move {
                let before_file = provider.open(PATH, OpenOptions::read_write()).await?;
                let writer_file = provider.open(PATH, OpenOptions::read_write()).await?;
                writer_file.write_at(0, b"new!").await?;
                let after_file = provider.open(PATH, OpenOptions::read_write()).await?;
                let after = after_file.sync_all().await.is_err();
                let before = before_file.sync_all().await.is_err();
                let writer = writer_file.sync_all().await.is_err();
                let last_file = provider.open(PATH, OpenOptions::read_write()).await?;
                let last = last_file.sync_all().await.is_err();
                Ok::<_, std::io::Error>((after, before, writer, last))
            });
            let seen = step_until_done(&mut sim, handle).await.expect("io");
            assert_eq!(
                seen,
                (after, before, writer, last),
                "{reporting:?}: (after, before, writer, last) sync errors"
            );
            sim.simulate_crash_for_process(test_ip(), true);
            assert_eq!(read_file(&mut sim).await, b"old!", "{reporting:?}");
        });
    }
}
