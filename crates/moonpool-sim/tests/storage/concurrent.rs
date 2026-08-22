//! Concurrent file operation tests for storage simulation.
//!
//! These tests verify that multiple files can be operated on simultaneously
//! without interference, similar to how network tests verify multiple
//! connections work independently.

use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimWorld, StorageConfiguration};
use std::net::IpAddr;

const TEST_IP_STR: &str = "127.0.0.1";

fn test_ip() -> IpAddr {
    TEST_IP_STR.parse().expect("valid IP")
}

/// Create a local tokio runtime for tests.
fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

/// Create a `SimWorld` with fast storage configuration.
fn fast_sim() -> SimWorld {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Test opening multiple files simultaneously
#[test]
fn test_multiple_files_open_simultaneously() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Open 5 files
            let mut files = Vec::new();
            for i in 0..5 {
                let file = provider
                    .open(&format!("multi_{i}.txt"), OpenOptions::create_write())
                    .await?;
                files.push(file);
            }

            // All files should be open
            assert_eq!(files.len(), 5);

            // Close them
            drop(files);
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test interleaved writes to multiple files
#[test]
fn test_interleaved_writes() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Open two files
            let mut file_a = provider
                .open("file_a.txt", OpenOptions::create_write())
                .await?;
            let mut file_b = provider
                .open("file_b.txt", OpenOptions::create_write())
                .await?;

            // Interleave writes
            file_a.write_all(b"A1").await?;
            file_b.write_all(b"B1").await?;
            file_a.write_all(b"A2").await?;
            file_b.write_all(b"B2").await?;
            file_a.write_all(b"A3").await?;
            file_b.write_all(b"B3").await?;

            file_a.sync_all().await?;
            file_b.sync_all().await?;

            drop(file_a);
            drop(file_b);

            // Verify file A content
            let mut file_a = provider
                .open("file_a.txt", OpenOptions::read_only())
                .await?;
            let mut buf_a = Vec::new();
            file_a.read_to_end(&mut buf_a).await?;
            assert_eq!(&buf_a, b"A1A2A3", "File A should have correct content");

            // Verify file B content
            let mut file_b = provider
                .open("file_b.txt", OpenOptions::read_only())
                .await?;
            let mut buf_b = Vec::new();
            file_b.read_to_end(&mut buf_b).await?;
            assert_eq!(&buf_b, b"B1B2B3", "File B should have correct content");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Concurrent append handles resolve EOF in completion order, so neither write
/// overwrites the other even when both operations have the same deadline.
#[test]
fn concurrent_append_handles_preserve_both_writes() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let verify_provider = provider.clone();
            let left_provider = provider.clone();
            let left = async move {
                let mut file = left_provider
                    .open("append.txt", OpenOptions::new().append(true).create(true))
                    .await?;
                file.write_all(b"A").await
            };
            let right = async move {
                let mut file = provider
                    .open("append.txt", OpenOptions::new().append(true).create(true))
                    .await?;
                file.write_all(b"B").await
            };
            let (left_result, right_result) = futures::join!(left, right);
            left_result?;
            right_result?;

            let mut file = verify_provider
                .open("append.txt", OpenOptions::read_only())
                .await?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents).await?;
            assert_eq!(contents, b"AB", "same-time appends must complete FIFO");
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// A read event captures its bytes before a later same-deadline write event,
/// even if the task is not repolled until both events have fired.
#[test]
fn read_completion_precedes_later_write_before_repoll() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let mut initial = provider
                .open("ordered.txt", OpenOptions::create_write())
                .await?;
            initial.write_all(b"old").await?;
            initial.sync_all().await?;
            drop(initial);

            let mut reader = provider
                .open("ordered.txt", OpenOptions::read_only())
                .await?;
            let mut writer = provider
                .open("ordered.txt", OpenOptions::new().write(true))
                .await?;
            let read = async move {
                let mut bytes = [0; 3];
                reader.read_exact(&mut bytes).await?;
                Ok::<_, std::io::Error>(bytes)
            };
            let write = async move { writer.write_all(b"new").await };
            let (read_result, write_result) = futures::join!(read, write);
            assert_eq!(&read_result?, b"old");
            write_result?;
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test writing to one file while reading from another (in same operation block)
#[test]
fn test_write_one_read_another() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Create the source file first
            let mut source = provider
                .open("read_source.txt", OpenOptions::create_write())
                .await?;
            source.write_all(b"source data for reading").await?;
            source.sync_all().await?;
            drop(source);

            // Now write to target while reading from source
            let mut write_file = provider
                .open("write_target.txt", OpenOptions::create_write())
                .await?;
            let mut read_file = provider
                .open("read_source.txt", OpenOptions::read_only())
                .await?;

            // Interleave read and write operations
            let mut buf = [0u8; 6];
            read_file.read_exact(&mut buf).await?;
            write_file.write_all(&buf).await?;

            read_file.read_exact(&mut buf).await?;
            write_file.write_all(&buf).await?;

            write_file.sync_all().await?;
            drop(write_file);
            drop(read_file);

            // Verify the write target
            let mut verify_file = provider
                .open("write_target.txt", OpenOptions::read_only())
                .await?;
            let mut result = Vec::new();
            verify_file.read_to_end(&mut result).await?;
            assert_eq!(&result, b"source data ");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test multiple sequential opens of the same file
#[test]
fn test_sequential_opens_same_file() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Write initial content
            let mut file = provider
                .open("reopen.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"initial").await?;
            file.sync_all().await?;
            drop(file);

            // Reopen and append (using read_write with append)
            let mut file = provider
                .open("reopen.txt", OpenOptions::new().read(true).write(true))
                .await?;
            // Seek to end
            file.seek(std::io::SeekFrom::End(0)).await?;
            file.write_all(b"_appended").await?;
            file.sync_all().await?;
            drop(file);

            // Read back
            let mut file = provider
                .open("reopen.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            assert_eq!(&buf, b"initial_appended");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test operations don't interfere across different files
#[test]
fn test_file_independence() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Create three files with different content
            for i in 0..3 {
                let mut file = provider
                    .open(&format!("independent_{i}.txt"), OpenOptions::create_write())
                    .await?;
                let content = format!("Content for file {i}");
                file.write_all(content.as_bytes()).await?;
                file.sync_all().await?;
                drop(file);
            }

            // Delete middle file
            provider.delete("independent_1.txt").await?;

            // Rename first file
            provider
                .rename("independent_0.txt", "renamed_0.txt")
                .await?;

            // Verify states:
            // - File 0: renamed
            // - File 1: deleted
            // - File 2: unchanged
            assert!(
                !provider.exists("independent_0.txt").await?,
                "Old name should not exist"
            );
            assert!(
                provider.exists("renamed_0.txt").await?,
                "New name should exist"
            );
            assert!(
                !provider.exists("independent_1.txt").await?,
                "Deleted file should not exist"
            );
            assert!(
                provider.exists("independent_2.txt").await?,
                "Unchanged file should exist"
            );

            // Verify content of unchanged file
            let mut file = provider
                .open("independent_2.txt", OpenOptions::read_only())
                .await?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            assert_eq!(&buf, b"Content for file 2");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test concurrent operations with different file positions
#[test]
fn test_independent_file_positions() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Create two files with same content
            for name in &["pos_a.txt", "pos_b.txt"] {
                let mut file = provider.open(name, OpenOptions::create_write()).await?;
                file.write_all(b"0123456789").await?;
                file.sync_all().await?;
                drop(file);
            }

            // Open both for reading
            let mut file_a = provider.open("pos_a.txt", OpenOptions::read_only()).await?;
            let mut file_b = provider.open("pos_b.txt", OpenOptions::read_only()).await?;

            // Seek to different positions
            file_a.seek(std::io::SeekFrom::Start(2)).await?;
            file_b.seek(std::io::SeekFrom::Start(7)).await?;

            // Read from both - should get different data due to different positions
            let mut buf_a = [0u8; 3];
            let mut buf_b = [0u8; 3];
            file_a.read_exact(&mut buf_a).await?;
            file_b.read_exact(&mut buf_b).await?;

            assert_eq!(&buf_a, b"234", "File A should read from position 2");
            assert_eq!(&buf_b, b"789", "File B should read from position 7");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test many files (stress test)
#[test]
fn test_many_files() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let file_count = 20;

            // Create many files
            for i in 0..file_count {
                let mut file = provider
                    .open(&format!("many_{i:03}.txt"), OpenOptions::create_write())
                    .await?;
                file.write_all(format!("File number {i}").as_bytes())
                    .await?;
                file.sync_all().await?;
                drop(file);
            }

            // Verify all exist
            let mut count = 0;
            for i in 0..file_count {
                if provider.exists(&format!("many_{i:03}.txt")).await? {
                    count += 1;
                }
            }
            assert_eq!(count, file_count, "All {file_count} files should exist");

            // Delete half
            for i in (0..file_count).step_by(2) {
                provider.delete(&format!("many_{i:03}.txt")).await?;
            }

            // Verify half remain
            let mut remaining = 0;
            for i in 0..file_count {
                if provider.exists(&format!("many_{i:03}.txt")).await? {
                    remaining += 1;
                }
            }
            assert_eq!(remaining, file_count / 2, "Half the files should remain");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Test reading from the same file sequentially (verifying data survives re-open)
#[test]
fn test_sequential_reads_same_file() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();

        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            // Create the file
            let mut file = provider
                .open("shared_read.txt", OpenOptions::create_write())
                .await?;
            file.write_all(b"shared content for reading").await?;
            file.sync_all().await?;
            drop(file);

            // First read
            let mut reader1 = provider
                .open("shared_read.txt", OpenOptions::read_only())
                .await?;
            let mut buf1 = Vec::new();
            reader1.read_to_end(&mut buf1).await?;
            drop(reader1);

            // Second read (re-open)
            let mut reader2 = provider
                .open("shared_read.txt", OpenOptions::read_only())
                .await?;
            let mut buf2 = Vec::new();
            reader2.read_to_end(&mut buf2).await?;
            drop(reader2);

            // Both reads should see same content
            assert_eq!(buf1, buf2, "Both reads should see same content");
            assert_eq!(&buf1, b"shared content for reading");

            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Two handles for one persistent file keep independent cursors.
#[test]
fn same_file_handles_have_independent_positions() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let mut writer = provider
                .open("shared_positions.txt", OpenOptions::create_write())
                .await?;
            writer.write_all(b"0123456789").await?;
            writer.sync_all().await?;
            drop(writer);

            let mut first = provider
                .open("shared_positions.txt", OpenOptions::read_only())
                .await?;
            let mut second = provider
                .open("shared_positions.txt", OpenOptions::read_only())
                .await?;
            first.seek(std::io::SeekFrom::Start(1)).await?;
            second.seek(std::io::SeekFrom::Start(7)).await?;

            let mut first_bytes = [0; 2];
            let mut second_bytes = [0; 2];
            futures::future::try_join(
                first.read_exact(&mut first_bytes),
                second.read_exact(&mut second_bytes),
            )
            .await?;

            assert_eq!(&first_bytes, b"12");
            assert_eq!(&second_bytes, b"78");
            assert_eq!(first.seek(std::io::SeekFrom::Current(0)).await?, 3);
            assert_eq!(second.seek(std::io::SeekFrom::Current(0)).await?, 9);
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Same-kind operations on one persistent file complete their exact requests,
/// even when both handles have work in flight together.
#[test]
fn concurrent_same_file_writes_keep_operation_identity() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let mut initial = provider
                .open("shared_writes.txt", OpenOptions::create_write())
                .await?;
            initial.write_all(b"............").await?;
            drop(initial);

            let options = OpenOptions::new().read(true).write(true);
            let mut first = provider.open("shared_writes.txt", options.clone()).await?;
            let mut second = provider.open("shared_writes.txt", options).await?;
            first.seek(std::io::SeekFrom::Start(1)).await?;
            second.seek(std::io::SeekFrom::Start(8)).await?;
            futures::future::try_join(first.write_all(b"AAA"), second.write_all(b"BB")).await?;

            drop(first);
            drop(second);
            let mut reader = provider
                .open("shared_writes.txt", OpenOptions::read_only())
                .await?;
            let mut contents = Vec::new();
            reader.read_to_end(&mut contents).await?;
            assert_eq!(&contents, b".AAA....BB..");
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}

/// Closing one handle invalidates that handle without closing the persistent
/// file or another independently opened handle.
#[test]
fn close_is_handle_local_and_enforced() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let provider = sim.storage_provider(test_ip());
        let handle = tokio::spawn(async move {
            let mut closed = provider
                .open("close_handle.txt", OpenOptions::create_write())
                .await?;
            closed.write_all(b"data").await?;
            let survivor = provider
                .open("close_handle.txt", OpenOptions::read_only())
                .await?;

            closed.close().await?;
            let error = closed.size().await.expect_err("closed handle must fail");
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(survivor.size().await?, 4);
            Ok::<_, std::io::Error>(())
        });

        while !handle.is_finished() {
            while sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }
        handle.await.expect("task panicked").expect("io error");
    });
}
