//! `BlockFile` over the real filesystem.
//!
//! The same block layer the simulation exercises, on a real file: it is
//! generic over `StorageFile`, so there is one implementation and two
//! backends rather than a production block device and a simulated one.

use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};
use tempfile::TempDir;

const BLOCK: usize = 4096;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

#[test]
fn block_round_trip_on_a_real_file() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("pages.db");
        let path = path.to_str().expect("temp path is valid UTF-8");

        let provider = moonpool_core::TokioStorageProvider::new();
        let file = provider
            .open(path, OpenOptions::create_write().read(true))
            .await
            .expect("open failed");
        let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");

        blocks.grow_to_blocks(8).await.expect("grow failed");
        assert_eq!(blocks.size_in_blocks().await.expect("size"), 8);
        // Growing to a size the file already has leaves it alone.
        blocks.grow_to_blocks(2).await.expect("grow failed");
        assert_eq!(blocks.size_in_blocks().await.expect("size"), 8);

        let mut page = blocks.buffer(2);
        page.as_mut_slice().fill(0x3B);
        blocks
            .write_blocks(3, page.as_slice())
            .await
            .expect("write");
        blocks.sync().await.expect("sync");

        let mut read = blocks.buffer(2);
        blocks
            .read_blocks(3, read.as_mut_slice())
            .await
            .expect("read");
        assert_eq!(read.as_slice(), page.as_slice());

        // The neighbouring block was never written and must be untouched.
        let mut neighbour = blocks.buffer(1);
        blocks
            .read_blocks(5, neighbour.as_mut_slice())
            .await
            .expect("read");
        assert!(neighbour.iter().all(|byte| *byte == 0));
    });
}

#[test]
fn reading_past_the_end_of_a_real_file_is_unexpected_eof() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("short.db");
        let path = path.to_str().expect("temp path is valid UTF-8");

        let provider = moonpool_core::TokioStorageProvider::new();
        let file = provider
            .open(path, OpenOptions::create_write().read(true))
            .await
            .expect("open failed");
        let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");
        blocks.grow_to_blocks(1).await.expect("grow failed");

        let mut buf = blocks.buffer(3);
        let error = blocks
            .read_blocks(0, buf.as_mut_slice())
            .await
            .expect_err("a read past the end must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    });
}

/// Direct I/O where the filesystem provides it; a documented fallback where it
/// does not. Either way the block layer works, and the buffers it hands out
/// are the ones the open demands.
#[test]
fn block_io_works_under_the_direct_io_policy() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("direct.db");
        let path = path.to_str().expect("temp path is valid UTF-8");

        let provider = moonpool_core::TokioStorageProvider::new();
        let file = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Optional),
            )
            .await
            .expect("open failed");
        let direct = file.is_direct_io();
        let blocks = BlockFile::new(file, BLOCK).expect("wrap failed");

        let mut page = blocks.buffer(1);
        page.as_mut_slice().fill(0xD4);
        blocks
            .write_blocks(0, page.as_slice())
            .await
            .expect("write");
        blocks.sync().await.expect("sync");

        let mut read = blocks.buffer(1);
        blocks
            .read_blocks(0, read.as_mut_slice())
            .await
            .expect("read");
        assert_eq!(read.as_slice(), page.as_slice());

        if direct {
            assert_eq!(blocks.constraints().memory_alignment(), BLOCK);
        } else {
            assert!(blocks.constraints().is_unconstrained());
        }
    });
}
