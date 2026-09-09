//! `BlockFile` over the simulated file backend.
//!
//! The block layer is deliberately boring: block arithmetic, whole transfers,
//! and nothing else. These tests pin exactly that — including the things it
//! must *not* do, which is most of what the region-based block device used to.

use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};
use std::io::SeekFrom;
use std::net::IpAddr;

const BLOCK: usize = 4096;

fn test_ip() -> IpAddr {
    "127.0.0.1".parse().expect("valid IP")
}

fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

fn fast_sim() -> SimWorld {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

async fn run_on<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let provider = sim.storage_provider(test_ip());
    let handle = tokio::spawn(f(provider));
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

/// The ownership boundary: the caller opens a file, the block layer wraps it.
#[test]
fn blocks_are_addressed_by_index_over_an_open_file() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let result: std::io::Result<()> = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("pages.db", OpenOptions::create_write().read(true))
                .await?;
            let blocks = BlockFile::new(file, BLOCK)?;
            assert_eq!(blocks.block_size(), BLOCK);

            blocks.grow_to_blocks(4).await?;
            assert_eq!(blocks.size_in_blocks().await?, 4);

            let mut page = blocks.buffer(1);
            page.as_mut_slice().fill(0xC7);
            blocks.write_blocks(2, page.as_slice()).await?;
            blocks.sync().await?;

            let mut read = blocks.buffer(1);
            blocks.read_blocks(2, read.as_mut_slice()).await?;
            assert!(read.iter().all(|byte| *byte == 0xC7));
            Ok(())
        })
        .await;
        result.expect("block I/O failed");
    });
}

/// Whole transfers: the block layer loops over the short reads and writes the
/// file below is allowed to return, so its caller never sees a partial one.
#[test]
fn short_transfers_are_looped_over() {
    local_runtime().block_on(async {
        let mut config = StorageConfiguration::fast_local();
        config.short_transfer_probability = 1.0;
        let mut sim = SimWorld::new();
        sim.set_storage_config(config);

        let result: std::io::Result<()> = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("choppy.db", OpenOptions::create_write().read(true))
                .await?;
            let blocks = BlockFile::new(file, BLOCK)?;

            let mut written = blocks.buffer(4);
            for (index, byte) in written.as_mut_slice().iter_mut().enumerate() {
                *byte = u8::try_from(index % 251).expect("modulo fits in u8");
            }
            blocks.write_blocks(0, written.as_slice()).await?;

            let mut read = blocks.buffer(4);
            blocks.read_blocks(0, read.as_mut_slice()).await?;
            assert_eq!(
                read.as_slice(),
                written.as_slice(),
                "every byte must arrive despite the disk moving prefixes"
            );
            Ok(())
        })
        .await;
        result.expect("short-transfer test failed");
    });
}

/// Reading a range the file does not have is `UnexpectedEof`, not a short
/// read and not a buffer half-filled with stale bytes.
#[test]
fn reading_past_the_end_is_unexpected_eof() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let kind = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("small.db", OpenOptions::create_write().read(true))
                .await
                .expect("open failed");
            let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");
            blocks.grow_to_blocks(1).await.expect("grow failed");

            let mut buf = blocks.buffer(2);
            blocks
                .read_blocks(0, buf.as_mut_slice())
                .await
                .expect_err("a read past the end must fail")
                .kind()
        })
        .await;

        assert_eq!(kind, std::io::ErrorKind::UnexpectedEof);
    });
}

/// A transfer that is not a whole number of blocks is a caller bug, in both
/// directions.
#[test]
fn partial_block_transfers_are_rejected() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        run_on(&mut sim, |provider| async move {
            let file = provider
                .open("strict.db", OpenOptions::create_write().read(true))
                .await
                .expect("open failed");
            let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");
            blocks.grow_to_blocks(2).await.expect("grow failed");

            let mut half = vec![0u8; BLOCK / 2];
            assert_eq!(
                blocks
                    .read_blocks(0, &mut half)
                    .await
                    .expect_err("half a block")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert_eq!(
                blocks
                    .write_blocks(0, &half)
                    .await
                    .expect_err("half a block")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert_eq!(
                blocks
                    .read_blocks(0, &mut [])
                    .await
                    .expect_err("no blocks at all")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        })
        .await;
    });
}

/// The block size is the caller's unit, not the device's: any multiple of the
/// file's alignment is legal, and anything else is refused at construction.
#[test]
fn the_block_size_must_fit_the_files_alignment() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        run_on(&mut sim, |provider| async move {
            let direct = provider
                .open(
                    "direct.db",
                    OpenOptions::create_write()
                        .read(true)
                        .direct_io(DirectIo::Required),
                )
                .await
                .expect("open failed");
            let alignment = direct.constraints().length_alignment();

            // A page several times the device's transfer unit is ordinary.
            let big = BlockFile::new(direct, alignment * 4).expect("a multiple is fine");
            assert_eq!(big.block_size(), alignment * 4);

            let direct = provider
                .open(
                    "direct2.db",
                    OpenOptions::create_write()
                        .read(true)
                        .direct_io(DirectIo::Required),
                )
                .await
                .expect("open failed");
            assert_eq!(
                BlockFile::new(direct, alignment + 1)
                    .expect_err("a non-multiple must be refused")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );

            // A buffered file constrains nothing, so any positive size works.
            let buffered = provider
                .open("buffered.db", OpenOptions::create_write().read(true))
                .await
                .expect("open failed");
            assert!(BlockFile::new(buffered, 7).is_ok());

            let zero = provider
                .open("zero.db", OpenOptions::create_write())
                .await
                .expect("open failed");
            assert_eq!(
                BlockFile::new(zero, 0).expect_err("zero blocks").kind(),
                std::io::ErrorKind::InvalidInput
            );
        })
        .await;
    });
}

/// Direct I/O reaches the block layer as it reaches everything else: the
/// buffers this hands out are aligned, and it is still not durability.
#[test]
fn direct_io_blocks_use_aligned_buffers_and_still_need_a_sync() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let result: std::io::Result<()> = run_on(&mut sim, |provider| async move {
            let file = provider
                .open(
                    "engine.db",
                    OpenOptions::create_write()
                        .read(true)
                        .direct_io(DirectIo::Required),
                )
                .await?;
            let blocks = BlockFile::new(file, BLOCK)?;
            assert!(blocks.get_ref().is_direct_io());

            let mut page = blocks.buffer(2);
            page.as_mut_slice().fill(0x5E);
            // The write goes through: an aligned buffer, an aligned offset,
            // an aligned length.
            blocks.write_blocks(0, page.as_slice()).await?;

            let mut read = blocks.buffer(2);
            blocks.read_blocks(0, read.as_mut_slice()).await?;
            assert!(read.iter().all(|byte| *byte == 0x5E));

            // Uncached is not durable; the sync is what makes it so.
            blocks.sync().await?;
            Ok(())
        })
        .await;
        result.expect("direct block I/O failed");
    });
}

/// One file, one image: the block layer writes the same bytes the stream and
/// positioned APIs see, because it *is* those APIs.
#[test]
fn block_writes_are_visible_through_the_file_apis() {
    local_runtime().block_on(async {
        let mut sim = fast_sim();
        let result: std::io::Result<()> = run_on(&mut sim, |provider| async move {
            let file = provider
                .open("shared.db", OpenOptions::create_write().read(true))
                .await?;
            let blocks = BlockFile::new(file, BLOCK)?;
            let mut page = blocks.buffer(1);
            page.as_mut_slice()[..5].copy_from_slice(b"stamp");
            blocks.write_blocks(1, page.as_slice()).await?;

            // Positioned read of the same bytes, through a second handle.
            let other = provider.open("shared.db", OpenOptions::read_only()).await?;
            let mut buf = [0u8; 5];
            other.read_at(BLOCK as u64, &mut buf).await?;
            assert_eq!(&buf, b"stamp");

            // And through the stream cursor.
            let mut streamed = provider.open("shared.db", OpenOptions::read_only()).await?;
            streamed.seek(SeekFrom::Start(BLOCK as u64)).await?;
            let mut head = [0u8; 5];
            streamed.read_exact(&mut head).await?;
            assert_eq!(&head, b"stamp");

            // A stream write lands under the block layer just as readily.
            let mut writer = provider
                .open("shared.db", OpenOptions::new().write(true))
                .await?;
            writer.seek(SeekFrom::Start(0)).await?;
            writer.write_all(b"header").await?;
            let mut first = blocks.buffer(1);
            blocks.read_blocks(0, first.as_mut_slice()).await?;
            assert_eq!(&first.as_slice()[..6], b"header");
            Ok(())
        })
        .await;
        result.expect("shared image test failed");
    });
}

/// A block write is not atomic, and the crash model proves it: a multi-block
/// write torn across a crash is a state recovery code has to survive.
#[test]
fn a_block_write_is_not_a_crash_atomicity_unit() {
    let mut torn = false;

    for seed in 0..400_u64 {
        let is_torn = local_runtime().block_on(async {
            let mut sim = SimWorld::new_with_seed(seed);
            sim.set_storage_config(StorageConfiguration::fast_local());

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("torn.db", OpenOptions::create_write().read(true))
                    .await
                    .expect("open failed");
                let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");
                let mut zeroes = blocks.buffer(2);
                blocks
                    .write_blocks(0, zeroes.as_slice())
                    .await
                    .expect("write failed");
                blocks.sync().await.expect("sync failed");

                zeroes.as_mut_slice().fill(0xFF);
                blocks
                    .write_blocks(0, zeroes.as_slice())
                    .await
                    .expect("write failed");
                // No sync: the crash may keep any part of it.
            })
            .await;

            sim.simulate_crash_for_process(test_ip(), true);

            run_on(&mut sim, |provider| async move {
                let file = provider
                    .open("torn.db", OpenOptions::read_only())
                    .await
                    .expect("open failed");
                let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");
                let mut buf = blocks.buffer(2);
                if blocks.read_blocks(0, buf.as_mut_slice()).await.is_err() {
                    return false;
                }
                // Torn: some of the new write landed and some did not.
                let kept_new = buf.contains(&0xFF);
                let kept_old = buf.contains(&0x00);
                kept_new && kept_old
            })
            .await
        });

        if is_torn {
            torn = true;
            break;
        }
    }

    assert!(
        torn,
        "a multi-block write must be able to tear across a crash"
    );
}
