//! Open semantics of the production `StorageProvider`.
//!
//! The direct-I/O policy must not change what an open *means*. These tests
//! pin the boundary: `Optional` may fall back to buffered I/O, and may not
//! turn any other failure into a success, weaken `create_new`, or create a
//! file the caller did not ask it to create.

use moonpool_core::{
    AlignedBuf, DirectIo, OpenOptions, StorageFile, StorageProvider, TokioStorageProvider,
};
use tempfile::TempDir;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

/// `create_new` means "fail if it exists" under every direct-I/O policy. The
/// bug this pins: attempting `O_DIRECT` first, having the kernel create the
/// file and *then* reject the flag, and retrying without the exclusivity the
/// caller asked for — which silently opens the existing file instead.
#[test]
fn optional_direct_io_does_not_weaken_create_new() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("exclusive.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        // Someone else already owns this name, with contents.
        let first = provider
            .open(path, OpenOptions::create_new_write())
            .await
            .expect("first create_new must succeed");
        first.write_at(0, b"original").await.expect("write failed");
        first.sync_all().await.expect("sync failed");
        drop(first);

        for policy in [DirectIo::Disabled, DirectIo::Optional, DirectIo::Required] {
            let error = provider
                .open(path, OpenOptions::create_new_write().direct_io(policy))
                .await
                .err()
                .map(|error| error.kind());
            assert!(
                matches!(
                    error,
                    Some(std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported)
                ),
                "create_new on an existing file must fail under {policy:?}, got {error:?}"
            );
        }

        // And the contents are still there: no attempt truncated or replaced
        // the file it refused to create.
        let survivor = provider
            .open(path, OpenOptions::read_only())
            .await
            .expect("open failed");
        let mut buf = [0u8; 8];
        assert_eq!(survivor.read_at(0, &mut buf).await.expect("read"), 8);
        assert_eq!(&buf, b"original");
    });
}

/// An ordinary open failure stays that failure. `Optional` must not retry its
/// way into a success, nor create a file for a caller that did not ask for
/// one.
#[test]
fn optional_direct_io_preserves_ordinary_open_failures() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("nonexistent.db");
        let missing = missing.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let error = provider
            .open(
                missing,
                OpenOptions::read_only().direct_io(DirectIo::Optional),
            )
            .await
            .expect_err("opening a missing file without create must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            !provider.exists(missing).await.expect("exists failed"),
            "a failed open must not leave a file behind"
        );

        // A directory is not a file to write to, under any policy.
        let as_file = dir.path().to_str().expect("temp path is valid UTF-8");
        let error = provider
            .open(
                as_file,
                OpenOptions::new().write(true).direct_io(DirectIo::Optional),
            )
            .await
            .expect_err("opening a directory for writing must fail");
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "the directory exists; the failure is about writing to it"
        );
    });
}

/// A truncating open truncates exactly once. If the direct attempt were made
/// first and then retried, the file would be truncated twice — harmless here,
/// but the same double-application is what makes `create_new` unsafe.
#[test]
fn optional_direct_io_applies_truncation_once() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("truncated.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let seeded = provider
            .open(path, OpenOptions::create_write())
            .await
            .expect("open failed");
        seeded.write_at(0, &[0xAB; 4096]).await.expect("write");
        seeded.sync_all().await.expect("sync");
        drop(seeded);

        let reopened = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Optional),
            )
            .await
            .expect("open failed");
        assert_eq!(reopened.size().await.expect("size"), 0);

        // The handle is usable whichever way the upgrade went — through a
        // buffer built for whatever constraints it ended up with.
        let constraints = reopened.constraints();
        let block = AlignedBuf::for_constraints(constraints.length_alignment(), constraints);
        reopened
            .write_at(0, block.as_slice())
            .await
            .expect("write failed");
    });
}

/// A direct-I/O file must advertise constraints its own device will honour.
/// The alignment is discovered, not assumed: this proves the reported values
/// are actually accepted, which a hard-coded guess cannot.
#[test]
fn direct_io_constraints_describe_what_the_device_accepts() {
    runtime().block_on(async {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("aligned.db");
        let path = path.to_str().expect("temp path is valid UTF-8");
        let provider = TokioStorageProvider::new();

        let file = provider
            .open(
                path,
                OpenOptions::create_write()
                    .read(true)
                    .direct_io(DirectIo::Optional),
            )
            .await
            .expect("open failed");

        let constraints = file.constraints();
        if !file.is_direct_io() {
            assert!(
                constraints.is_unconstrained(),
                "a buffered file constrains nothing"
            );
            return;
        }

        for alignment in [
            constraints.offset_alignment(),
            constraints.length_alignment() as u64,
            constraints.memory_alignment() as u64,
        ] {
            assert!(
                alignment.is_power_of_two(),
                "{alignment} is not a power of two"
            );
        }

        // A transfer built to exactly the reported alignment must be accepted
        // by the device. If the numbers were too weak, this is where the
        // kernel would return EINVAL.
        let length = constraints.length_alignment();
        let mut block = AlignedBuf::for_constraints(length, constraints);
        block.as_mut_slice().fill(0xE1);
        assert_eq!(
            file.write_at(constraints.offset_alignment(), block.as_slice())
                .await
                .expect("a transfer at the reported alignment must be accepted"),
            length
        );

        let mut read = AlignedBuf::for_constraints(length, constraints);
        assert_eq!(
            file.read_at(constraints.offset_alignment(), read.as_mut_slice())
                .await
                .expect("read failed"),
            length
        );
        assert_eq!(read.as_slice(), block.as_slice());
    });
}
