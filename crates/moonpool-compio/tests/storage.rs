//! `CompioStorageProvider` on the real filesystem.
//!
//! Temporary directories live under the target directory, not `/tmp`, which
//! can be tmpfs (no direct I/O there).

use std::io::{self, SeekFrom};

use futures::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use moonpool_compio::CompioStorageProvider;
use moonpool_core::{BlockFile, DirectIo, OpenOptions, StorageFile, StorageProvider};
use tempfile::TempDir;

fn temp_dir() -> TempDir {
    TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).expect("temp dir")
}

fn path(dir: &TempDir, name: &str) -> String {
    dir.path()
        .join(name)
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

/// The namespace operations and positioned I/O on a buffered file.
async fn namespace_and_positioned_io() -> io::Result<()> {
    let dir = temp_dir();
    let storage = CompioStorageProvider::new();
    let parent = path(&dir, "a");
    let nested = path(&dir, "a/b");
    storage.create_dir_all(&nested).await?;
    storage.sync_dir(&parent).await?;

    let data = format!("{nested}/data");
    assert!(!storage.exists(&data).await?);
    let file = storage
        .open(
            &data,
            OpenOptions::new().read(true).write(true).create(true),
        )
        .await?;
    assert!(storage.exists(&data).await?);
    assert!(!file.is_direct_io());
    assert!(file.constraints().is_unconstrained());

    assert_eq!(file.write_at(10, b"hello").await?, 5);
    let mut buf = [0xff_u8; 15];
    assert_eq!(file.read_at(0, &mut buf).await?, 15);
    assert_eq!(&buf[..10], &[0; 10], "the gap reads as zeroes");
    assert_eq!(&buf[10..], b"hello");
    file.sync_all().await?;
    file.sync_data().await?;
    assert_eq!(file.size().await?, 15);
    file.set_len(4).await?;
    assert_eq!(file.size().await?, 4);
    file.set_len(100).await?;
    assert_eq!(file.size().await?, 100);
    assert_eq!(file.read_at(100, &mut buf).await?, 0, "end of file");
    drop(file);

    let renamed = format!("{nested}/renamed");
    storage.rename(&data, &renamed).await?;
    assert!(!storage.exists(&data).await?);
    assert!(storage.exists(&renamed).await?);
    storage
        .open(&format!("{nested}/another"), OpenOptions::create_write())
        .await?;
    assert_eq!(storage.list_dir(&nested).await?, ["another", "renamed"]);
    storage.sync_dir(&nested).await?;
    assert_eq!(
        storage.sync_dir(&renamed).await.map_err(|e| e.kind()),
        Err(io::ErrorKind::NotADirectory)
    );
    assert_eq!(
        storage.list_dir(&renamed).await.map_err(|e| e.kind()),
        Err(io::ErrorKind::NotADirectory)
    );
    assert_eq!(
        storage
            .list_dir(&path(&dir, "missing"))
            .await
            .map_err(|e| e.kind()),
        Err(io::ErrorKind::NotFound)
    );
    storage.delete(&renamed).await?;
    assert!(!storage.exists(&renamed).await?);
    assert_eq!(
        storage.delete(&renamed).await.map_err(|e| e.kind()),
        Err(io::ErrorKind::NotFound)
    );
    Ok(())
}

#[compio::test]
async fn the_namespace_and_positioned_io_work() -> io::Result<()> {
    namespace_and_positioned_io().await
}

/// Stream I/O on a buffered file: writes, seeks and reads share one cursor,
/// and positioned I/O and stream I/O see one file.
async fn stream_io_on_a_buffered_file() -> io::Result<()> {
    let dir = temp_dir();
    let storage = CompioStorageProvider::new();
    let name = path(&dir, "stream");
    let mut file = storage
        .open(&name, OpenOptions::create_write().read(true))
        .await?;
    file.write_all(b"hello world").await?;
    file.flush().await?;
    assert_eq!(file.seek(SeekFrom::Start(6)).await?, 6);
    let mut tail = String::new();
    file.read_to_string(&mut tail).await?;
    assert_eq!(tail, "world");
    assert_eq!(file.seek(SeekFrom::End(-5)).await?, 6);
    assert_eq!(file.seek(SeekFrom::Current(-1)).await?, 5);
    assert_eq!(
        file.seek(SeekFrom::Current(-10))
            .await
            .map_err(|e| e.kind()),
        Err(io::ErrorKind::InvalidInput)
    );
    file.write_at(0, b"HELLO").await?;
    file.seek(SeekFrom::Start(0)).await?;
    let mut all = Vec::new();
    file.read_to_end(&mut all).await?;
    assert_eq!(all, b"HELLO world");
    file.close().await?;
    drop(file);

    // Append: stream writes go to the end, positioned writes where told.
    let mut appending = storage
        .open(&name, OpenOptions::new().read(true).append(true))
        .await?;
    appending.write_all(b"!").await?;
    appending.write_at(0, b"h").await?;
    appending.write_all(b"?").await?;
    let mut all = Vec::new();
    appending.seek(SeekFrom::Start(0)).await?;
    appending.read_to_end(&mut all).await?;
    assert_eq!(all, b"hELLO world!?");
    Ok(())
}

#[compio::test]
async fn stream_io_works_on_a_buffered_file() -> io::Result<()> {
    stream_io_on_a_buffered_file().await
}

/// Direct I/O through `BlockFile`: `Optional` (direct when the filesystem
/// offers it), the zero-copy owned path and the borrowed path, the stream
/// API refused on a direct file, and `Required`.
#[compio::test]
async fn direct_io_round_trips_through_a_block_file() -> io::Result<()> {
    let dir = temp_dir();
    let storage = CompioStorageProvider::new();
    let name = path(&dir, "blocks");
    let file = storage
        .open(
            &name,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .direct_io(DirectIo::Optional),
        )
        .await?;
    let direct = file.is_direct_io();
    eprintln!(
        "direct I/O under {}: {direct} ({:?})",
        dir.path().display(),
        file.constraints()
    );
    let blocks = BlockFile::new(file, 4096)?;

    let mut owned = blocks.buffer(2)?;
    for (at, byte) in owned.as_mut_slice().iter_mut().enumerate() {
        *byte = u8::try_from(at % 251).expect("fits");
    }
    let expected = owned.as_slice().to_vec();
    blocks.write_blocks_durable_aligned(0, owned).await?;
    let back = blocks.read_blocks_aligned(0, blocks.buffer(2)?).await?;
    assert_eq!(back.as_slice(), expected.as_slice());

    // The borrowed path, through the aligned bounce buffer.
    let mut borrowed = blocks.buffer(1)?;
    borrowed.as_mut_slice().fill(0xa5);
    blocks.write_blocks(2, borrowed.as_slice()).await?;
    let mut read = blocks.buffer(1)?;
    blocks.read_blocks(2, read.as_mut_slice()).await?;
    assert!(read.as_slice().iter().all(|&b| b == 0xa5));
    blocks.sync().await?;

    let mut file = blocks.into_inner();
    if direct {
        let mut misaligned = [0_u8; 7];
        assert_eq!(
            file.read_at(1, &mut misaligned).await.map_err(|e| e.kind()),
            Err(io::ErrorKind::InvalidInput)
        );
        let mut byte = [0_u8; 1];
        assert_eq!(
            file.read(&mut byte).await.map_err(|e| e.kind()),
            Err(io::ErrorKind::Unsupported),
            "stream reads are refused on a direct file"
        );
        assert_eq!(
            file.write(b"x").await.map_err(|e| e.kind()),
            Err(io::ErrorKind::Unsupported),
            "stream writes are refused on a direct file"
        );
    }
    drop(file);

    // `Required` on the existing file: direct, or the documented refusal.
    let required = storage
        .open(
            &name,
            OpenOptions::read_write().direct_io(DirectIo::Required),
        )
        .await;
    match required {
        Ok(file) => {
            assert!(direct, "Required succeeded where Optional fell back");
            assert!(file.is_direct_io());
        }
        Err(error) => {
            assert!(!direct, "Required failed where Optional got direct I/O");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        }
    }
    // `Required` never creates a file.
    let missing = path(&dir, "missing");
    assert_eq!(
        storage
            .open(
                &missing,
                OpenOptions::create_write().direct_io(DirectIo::Required)
            )
            .await
            .map(|_| ())
            .map_err(|e| e.kind()),
        Err(io::ErrorKind::Unsupported)
    );
    assert!(!storage.exists(&missing).await?);
    Ok(())
}

/// The same storage round trips on the polling driver (epoll plus the
/// blocking pool), forced through the proactor builder.
#[test]
fn storage_round_trips_on_the_polling_driver() -> io::Result<()> {
    let mut proactor = compio::driver::ProactorBuilder::new();
    proactor.driver_type(compio::driver::DriverType::Poll);
    let runtime = compio::runtime::RuntimeBuilder::new()
        .with_proactor(proactor)
        .build()?;
    assert_eq!(runtime.driver_type(), compio::driver::DriverType::Poll);
    runtime.block_on(async {
        namespace_and_positioned_io().await?;
        stream_io_on_a_buffered_file().await
    })
}
