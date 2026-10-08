//! The meta file: CLSTORE's metainfo, in two checksummed copies far apart
//! in one file.
//!
//! The copies are written **one after the other**, each followed by a sync,
//! and only once the commit's batch is durable. At most
//! one copy is ever in flight, so a crash tears at most the one being
//! written, and after a complete write both hold the same value, so a single
//! rotted copy never rolls the metainfo back (`LogCabin` and canonical/raft
//! alternate between their copies instead, which costs one write per update
//! and loses that guarantee).
//!
//! Opening picks the valid copy with the higher sequence number and rewrites
//! the other from it; two valid copies of one sequence that disagree are
//! damage, and so are two damaged copies: [`OpenError::MetaLost`].

use moonpool_core::{BlockFile, OpenOptions, StorageFile, StorageProvider};

use crate::format::{BLOCK, BLOCK_U64, META_COPY_B_AT, META_FILE_BLOCKS, Meta};
use crate::io::{read_blocks, write_blocks};
use crate::{JournalId, OpenError};

pub(crate) const META_NAME: &str = "meta";
const META_TMP: &str = "meta.tmp";

/// Which copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Copy {
    A = 0,
    B = 1,
}

impl Copy {
    fn block(self) -> u64 {
        match self {
            Copy::A => 0,
            Copy::B => META_COPY_B_AT / BLOCK_U64,
        }
    }
}

pub(crate) struct MetaFile<F> {
    file: BlockFile<F>,
}

impl<F: StorageFile> MetaFile<F> {
    /// Create the meta file with both copies of `meta`: written to a
    /// temporary name, synced, then published by a rename and a directory
    /// sync. The rename is the journal's birth: until it is durable, there
    /// is no journal here.
    pub async fn create<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        meta: &Meta,
    ) -> Result<Self, OpenError> {
        let temporary = format!("{dir}/{META_TMP}");
        if provider.exists(&temporary).await? {
            provider.delete(&temporary).await?;
        }
        let file = provider
            .open(&temporary, OpenOptions::create_new_write().read(true))
            .await?;
        let file = BlockFile::new(file, BLOCK)?;
        let mut image = vec![0u8; crate::format::usize_of(META_FILE_BLOCKS * BLOCK_U64)];
        meta.encode(Copy::A as u8, &mut image[..BLOCK]);
        let b_at = crate::format::usize_of(META_COPY_B_AT);
        meta.encode(Copy::B as u8, &mut image[b_at..b_at + BLOCK]);
        write_blocks(&file, 0, &image).await?;
        file.sync().await?;
        drop(file);
        provider
            .rename(&temporary, &format!("{dir}/{META_NAME}"))
            .await?;
        provider.sync_dir(dir).await?;
        Ok(Self {
            file: open_file(provider, dir).await?,
        })
    }

    /// Open the meta file and choose its current value. `Ok(None)`: there
    /// is no meta file (and the caller decides what that means). Returns the
    /// value and whether a copy was rewritten from its twin.
    pub async fn open<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        journal: JournalId,
    ) -> Result<Option<(Self, Meta, bool)>, OpenError> {
        if !provider.exists(&format!("{dir}/{META_NAME}")).await? {
            return Ok(None);
        }
        let file = open_file(provider, dir).await?;
        if file.size_in_blocks().await? != META_FILE_BLOCKS {
            return Err(OpenError::MetaLost);
        }
        let meta_file = Self { file };
        // A predecessor's unsynced copy may sit in the page cache (its commit
        // failed, or it restarted without a power loss). It reaches the disk
        // before the other copy is touched: two copies in flight at once can
        // both be torn by one crash.
        meta_file.sync().await?;
        let (meta, stale) = meta_file.choose(journal).await?;
        if let Some(copy) = stale {
            meta_file.write(copy, &meta).await?;
            meta_file.sync().await?;
        }
        Ok(Some((meta_file, meta, stale.is_some())))
    }

    /// The current value and the copy that is stale, if one is: the valid
    /// copy with the higher sequence number. Two damaged copies, or two valid
    /// copies of one sequence that disagree, are [`OpenError::MetaLost`].
    async fn choose(&self, journal: JournalId) -> Result<(Meta, Option<Copy>), OpenError> {
        let a = self.read(Copy::A).await?;
        let b = self.read(Copy::B).await?;
        let (meta, stale) = match (a, b) {
            (Some(a), Some(b)) if a == b => (a, None),
            (Some(a), Some(b)) if a.seq == b.seq => return Err(OpenError::MetaLost),
            (Some(a), Some(b)) if a.seq > b.seq => (a, Some(Copy::B)),
            (Some(a), None) => (a, Some(Copy::B)),
            (_, Some(b)) => (b, Some(Copy::A)),
            (None, None) => return Err(OpenError::MetaLost),
        };
        if meta.journal != journal {
            return Err(OpenError::WrongJournal {
                found: meta.journal.0,
            });
        }
        Ok((meta, stale))
    }

    /// The metainfo of a closed journal, read only: no sync, no repair,
    /// nothing created. `Ok(None)` where there is no meta file.
    pub async fn peek<P: StorageProvider<File = F>>(
        provider: &P,
        dir: &str,
        journal: JournalId,
    ) -> Result<Option<Meta>, OpenError> {
        let path = format!("{dir}/{META_NAME}");
        if !provider.exists(&path).await? {
            return Ok(None);
        }
        let file = provider.open(&path, OpenOptions::read_only()).await?;
        let file = BlockFile::new(file, BLOCK)?;
        if file.size_in_blocks().await? != META_FILE_BLOCKS {
            return Err(OpenError::MetaLost);
        }
        let (meta, _) = Self { file }.choose(journal).await?;
        Ok(Some(meta))
    }

    async fn read(&self, copy: Copy) -> std::io::Result<Option<Meta>> {
        let mut buf = self.file.buffer(1)?;
        read_blocks(&self.file, copy.block(), buf.as_mut_slice()).await?;
        Ok(Meta::decode(buf.as_slice(), copy as u8))
    }

    /// Write one copy. Not durable until [`sync`](Self::sync).
    pub async fn write(&self, copy: Copy, meta: &Meta) -> std::io::Result<()> {
        let mut block = vec![0u8; BLOCK];
        meta.encode(copy as u8, &mut block);
        write_blocks(&self.file, copy.block(), &block).await
    }

    /// Replace the value: copy A, synced, then copy B, synced, so at most
    /// one copy is ever in flight.
    pub async fn update(&self, meta: &Meta) -> std::io::Result<()> {
        self.write(Copy::A, meta).await?;
        self.sync().await?;
        self.write(Copy::B, meta).await?;
        self.sync().await
    }

    pub async fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data().await
    }
}

async fn open_file<P: StorageProvider>(
    provider: &P,
    dir: &str,
) -> std::io::Result<BlockFile<P::File>> {
    let file = provider
        .open(&format!("{dir}/{META_NAME}"), OpenOptions::read_write())
        .await?;
    BlockFile::new(file, BLOCK)
}

/// Remove a meta file a crashed `create` left under its temporary name.
pub(crate) async fn remove_leftover<P: StorageProvider>(
    provider: &P,
    dir: &str,
) -> std::io::Result<()> {
    let temporary = format!("{dir}/{META_TMP}");
    if provider.exists(&temporary).await? {
        provider.delete(&temporary).await?;
    }
    Ok(())
}
