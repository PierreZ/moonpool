//! Tests for the production filesystem `BlockDevice` implementation.

#![cfg(all(feature = "tokio-fs", unix))]

use moonpool_core::block::SECTOR_SIZE;
use moonpool_core::{
    BlockDevice, BlockDeviceProvider, BlockError, RegionId, RegionSpec, TokioBlockDeviceProvider,
};

const SECTOR: u64 = SECTOR_SIZE as u64;

fn specs() -> [RegionSpec; 2] {
    [
        RegionSpec {
            name: "wal",
            size: 8 * SECTOR,
        },
        RegionSpec {
            name: "superblock",
            size: SECTOR,
        },
    ]
}

#[tokio::test]
async fn create_is_invisible_until_first_persist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dev").display().to_string();
    let provider = TokioBlockDeviceProvider::new();

    let device = provider.create(&path, &specs()).await.expect("create");
    let err = provider.open(&path).await.expect_err("open before persist");
    assert!(matches!(err, BlockError::NotFound { .. }), "got {err:?}");

    device.persist().await.expect("first persist");
    let reopened = provider.open(&path).await.expect("open after persist");
    assert_eq!(reopened.region_count(), 2);
    assert_eq!(reopened.region_size(RegionId(0)), 8 * SECTOR);
    assert_eq!(reopened.region_size(RegionId(1)), SECTOR);

    let err = provider
        .create(&path, &specs())
        .await
        .expect_err("create over existing device");
    assert!(
        matches!(err, BlockError::AlreadyExists { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn write_read_roundtrip_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dev").display().to_string();
    let provider = TokioBlockDeviceProvider::new();

    let device = provider.create(&path, &specs()).await.expect("create");
    let payload = vec![0xAB; 2 * SECTOR_SIZE];
    device
        .write(RegionId(0), 3 * SECTOR, &payload)
        .await
        .expect("write");
    device
        .write(RegionId(1), 0, &vec![0xCD; SECTOR_SIZE])
        .await
        .expect("write superblock");
    device.persist().await.expect("persist");

    let reopened = provider.open(&path).await.expect("open");
    let mut buf = vec![0u8; 2 * SECTOR_SIZE];
    reopened
        .read(RegionId(0), 3 * SECTOR, &mut buf)
        .await
        .expect("read");
    assert_eq!(buf, payload);
    let mut sb = vec![0u8; SECTOR_SIZE];
    reopened.read(RegionId(1), 0, &mut sb).await.expect("read");
    assert_eq!(sb, vec![0xCD; SECTOR_SIZE]);
}

#[tokio::test]
async fn alignment_bounds_and_grow_rules_are_enforced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dev").display().to_string();
    let provider = TokioBlockDeviceProvider::new();
    let device = provider.create(&path, &specs()).await.expect("create");
    let region = RegionId(0);

    let mut buf = vec![0u8; SECTOR_SIZE];
    let misaligned = device.read(region, 1, &mut buf).await;
    assert!(matches!(
        misaligned,
        Err(BlockError::InvalidArgument { .. })
    ));
    let short = device.write(region, 0, &[0u8; 100]).await;
    assert!(matches!(short, Err(BlockError::InvalidArgument { .. })));
    let oob = device.write(region, 8 * SECTOR, &buf).await;
    assert!(matches!(oob, Err(BlockError::InvalidArgument { .. })));
    let unknown = device.write(RegionId(9), 0, &buf).await;
    assert!(matches!(unknown, Err(BlockError::InvalidArgument { .. })));

    let shrink = device.grow(region, SECTOR).await;
    assert!(matches!(shrink, Err(BlockError::InvalidArgument { .. })));
    device.grow(region, 16 * SECTOR).await.expect("grow");
    assert_eq!(device.region_size(region), 16 * SECTOR);
    device
        .write(region, 12 * SECTOR, &vec![0x77; SECTOR_SIZE])
        .await
        .expect("write into grown area");
    device.persist().await.expect("persist");

    // The grown size is durable after persist.
    let reopened = provider.open(&path).await.expect("open");
    assert_eq!(reopened.region_size(region), 16 * SECTOR);
    let mut grown = vec![0u8; SECTOR_SIZE];
    reopened
        .read(region, 12 * SECTOR, &mut grown)
        .await
        .expect("read grown");
    assert_eq!(grown, vec![0x77; SECTOR_SIZE]);
}

#[tokio::test]
async fn abandoned_staging_is_replaced_by_the_next_create() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dev").display().to_string();
    let provider = TokioBlockDeviceProvider::new();

    // A create that never persists models a crash mid-format.
    let abandoned = provider.create(&path, &specs()).await.expect("create");
    abandoned
        .write(RegionId(0), 0, &vec![0x11; SECTOR_SIZE])
        .await
        .expect("write");
    drop(abandoned);
    assert!(matches!(
        provider.open(&path).await,
        Err(BlockError::NotFound { .. })
    ));

    // The atomic-create contract says the abandoned attempt never existed.
    let device = provider.create(&path, &specs()).await.expect("re-create");
    device.persist().await.expect("persist");
    provider.open(&path).await.expect("open");
}
