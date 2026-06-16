//! Storage provider contract: the invariants any drop-in for `tokio::fs` must hold.
//!
//! Like the network streams, files implement the `futures::io` traits via
//! `Compat`, so seeking/reading/writing go through `futures::io` extension
//! traits while durability/size live on [`StorageFile`].

use crate::fixtures::Fixtures;
use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use moonpool_core::{OpenOptions, Providers, StorageFile, StorageProvider};
use std::io::SeekFrom;

/// Assert the [`StorageProvider`] round-trips data and reports filesystem errors.
pub(crate) async fn storage_contract<P: Providers, F: Fixtures>(p: &P, fixtures: &F) {
    let storage = p.storage();

    // write -> sync -> reopen -> read round-trip.
    let path = fixtures.path("roundtrip.txt");
    let mut file = storage
        .open(&path, OpenOptions::create_write())
        .await
        .expect("create for write");
    file.write_all(b"hello world").await.expect("write");
    file.sync_all().await.expect("sync_all");
    drop(file);

    let mut file = storage
        .open(&path, OpenOptions::read_only())
        .await
        .expect("reopen for read");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.expect("read_to_end");
    assert_eq!(buf, b"hello world", "round-tripped bytes must match");
    drop(file);

    // exists toggles around create and delete.
    assert!(storage.exists(&path).await.expect("exists"), "file exists");
    storage.delete(&path).await.expect("delete");
    assert!(
        !storage.exists(&path).await.expect("exists after delete"),
        "file gone after delete"
    );

    // rename moves content; the source disappears.
    let from = fixtures.path("from.txt");
    let to = fixtures.path("to.txt");
    let mut file = storage
        .open(&from, OpenOptions::create_write())
        .await
        .expect("create rename source");
    file.write_all(b"payload").await.expect("write source");
    file.sync_all().await.expect("sync source");
    drop(file);

    storage.rename(&from, &to).await.expect("rename");
    assert!(
        !storage.exists(&from).await.expect("source exists check"),
        "rename source must be gone"
    );
    assert!(
        storage.exists(&to).await.expect("dest exists check"),
        "rename dest must exist"
    );

    // seek + partial read, plus set_len truncate/extend reflected by size().
    let path = fixtures.path("seek.txt");
    let mut file = storage
        .open(&path, OpenOptions::create_write())
        .await
        .expect("create seek file");
    file.write_all(b"0123456789").await.expect("write digits");
    file.sync_all().await.expect("sync digits");
    assert_eq!(file.size().await.expect("size"), 10, "size after write");

    file.set_len(5).await.expect("truncate");
    assert_eq!(file.size().await.expect("size"), 5, "size after truncate");
    drop(file);

    let mut file = storage
        .open(&path, OpenOptions::read_only())
        .await
        .expect("reopen seek file");
    let pos = file.seek(SeekFrom::Start(2)).await.expect("seek");
    assert_eq!(pos, 2, "seek must report the new position");
    let mut one = [0u8; 1];
    file.read_exact(&mut one).await.expect("read after seek");
    assert_eq!(&one, b"2", "partial read after seek");
    drop(file);

    // Opening a missing file for read is NotFound. (The Ok type is the file
    // handle, which isn't Debug, so match instead of expect_err.)
    let missing = fixtures.path("missing.txt");
    let kind = match storage.open(&missing, OpenOptions::read_only()).await {
        Ok(_) => panic!("opening a missing file must error"),
        Err(err) => err.kind(),
    };
    assert_eq!(
        kind,
        std::io::ErrorKind::NotFound,
        "missing file must be NotFound"
    );

    // create_new on an existing file is AlreadyExists.
    let existing = fixtures.path("existing.txt");
    let file = storage
        .open(&existing, OpenOptions::create_write())
        .await
        .expect("create existing");
    drop(file);
    let kind = match storage
        .open(&existing, OpenOptions::create_new_write())
        .await
    {
        Ok(_) => panic!("create_new on an existing file must error"),
        Err(err) => err.kind(),
    };
    assert_eq!(
        kind,
        std::io::ErrorKind::AlreadyExists,
        "create_new over existing must be AlreadyExists"
    );
}
