//! `BlockFile` over the real filesystem.
//!
//! The same block layer the simulation exercises, on a real file: it is
//! generic over `StorageFile`, so there is one implementation and two
//! backends rather than a production block device and a simulated one.

use moonpool_core::{
    BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider, TokioStorageFile,
    TokioStorageProvider,
};
use tempfile::TempDir;

const BLOCK: usize = 4096;

/// Open `name` in `dir` with `options` and view it in [`BLOCK`]-byte blocks.
async fn open_blocks(
    dir: &TempDir,
    name: &str,
    options: OpenOptions,
) -> BlockFile<TokioStorageFile> {
    let path = dir.path().join(name);
    let path = path.to_str().expect("temp path is valid UTF-8");
    let file = TokioStorageProvider::new()
        .open(path, options)
        .await
        .expect("open failed");
    BlockFile::new(file, BLOCK).expect("wrap failed")
}

/// Write `count` blocks of `fill` at `index`, sync, and read them back.
async fn round_trip(blocks: &BlockFile<TokioStorageFile>, index: u64, count: usize, fill: u8) {
    let mut page = blocks.buffer(count).expect("buffer allocation");
    page.as_mut_slice().fill(fill);
    blocks
        .write_blocks(index, page.as_slice())
        .await
        .expect("write");
    blocks.sync().await.expect("sync");

    let mut read = blocks.buffer(count).expect("buffer allocation");
    blocks
        .read_blocks(index, read.as_mut_slice())
        .await
        .expect("read");
    assert_eq!(read.as_slice(), page.as_slice());
}

#[tokio::test]
async fn block_round_trip_on_a_real_file() {
    let dir = TempDir::new().expect("temp dir");
    let blocks = open_blocks(&dir, "pages.db", OpenOptions::create_write().read(true)).await;

    blocks.grow_to_blocks(8).await.expect("grow failed");
    assert_eq!(blocks.size_in_blocks().await.expect("size"), 8);
    // Growing to a size the file already has leaves it alone.
    blocks.grow_to_blocks(2).await.expect("grow failed");
    assert_eq!(blocks.size_in_blocks().await.expect("size"), 8);

    round_trip(&blocks, 3, 2, 0x3B).await;

    // The neighbouring block was never written and must be untouched.
    let mut neighbour = blocks.buffer(1).expect("buffer allocation");
    blocks
        .read_blocks(5, neighbour.as_mut_slice())
        .await
        .expect("read");
    assert!(neighbour.iter().all(|byte| *byte == 0));
}

#[tokio::test]
async fn reading_past_the_end_of_a_real_file_is_unexpected_eof() {
    let dir = TempDir::new().expect("temp dir");
    let blocks = open_blocks(&dir, "short.db", OpenOptions::create_write().read(true)).await;
    blocks.grow_to_blocks(1).await.expect("grow failed");

    let mut buf = blocks.buffer(3).expect("buffer allocation");
    let error = blocks
        .read_blocks(0, buf.as_mut_slice())
        .await
        .expect_err("a read past the end must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

/// Direct I/O where the filesystem provides it; a documented fallback where it
/// does not. Either way the block layer works, and the buffers it hands out
/// are the ones the open demands.
#[tokio::test]
async fn block_io_works_under_the_direct_io_policy() {
    let dir = TempDir::new().expect("temp dir");
    let options = OpenOptions::create_write()
        .read(true)
        .direct_io(DirectIo::Optional);
    let blocks = open_blocks(&dir, "direct.db", options).await;

    round_trip(&blocks, 0, 1, 0xD4).await;

    let constraints = blocks.constraints();
    if blocks.get_ref().is_direct_io() {
        // The device's requirement, whatever it is — not the block size.
        // A 4 KiB page over 512-byte transfers is the ordinary case, and
        // asserting equality here would be asserting the two are the same
        // concept.
        assert!(constraints.memory_alignment().is_power_of_two());
        assert!(
            BLOCK.is_multiple_of(constraints.length_alignment()),
            "a block must be a whole number of transfer units"
        );
    } else {
        assert!(constraints.is_unconstrained());
    }
}
