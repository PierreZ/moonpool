//! A small record kept in two copies, `<name>.0` and `<name>.1`.
//!
//! Each copy carries a generation counter and a CRC. An update rewrites each
//! copy in turn — `.0`, then `.1` — through a temporary file, a sync, a
//! rename, and a directory sync, so at every instant at least one copy holds
//! either the old value or the new one intact. Loading takes the valid copy
//! with the highest generation.
//!
//! ```text
//! 0   u32 magic       8   u64 generation     20  u32 crc (0..20 + payload)
//! 4   u32 version     16  u32 payload length 24  payload …, zero-padded to a block
//! ```

use moonpool_core::{BlockFile, OpenOptions, StorageFile, StorageProvider};

use crate::JournalError;
use crate::layout::{BLOCK, BLOCK_U64};

const MAGIC: u32 = u32::from_le_bytes(*b"MPJM");
const VERSION: u32 = 1;
const HEADER: usize = 24;

/// One two-copy record under a directory.
#[derive(Debug)]
pub(crate) struct DualFile {
    dir: String,
    name: &'static str,
    /// Generation of the newest valid copy; zero when none exists.
    generation: u64,
}

fn crc(header: &[u8], payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&header[..20]), payload)
}

/// Decode one copy into `(generation, payload)`, or `None` if it is damaged.
fn decode(bytes: &[u8]) -> Option<(u64, Vec<u8>)> {
    if bytes.len() < HEADER {
        return None;
    }
    let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
    if field(0) != MAGIC || field(4) != VERSION {
        return None;
    }
    let generation = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let len = field(16) as usize;
    let payload = bytes.get(HEADER..HEADER.checked_add(len)?)?;
    (crc(bytes, payload) == field(20)).then(|| (generation, payload.to_vec()))
}

impl DualFile {
    fn path(&self, copy: u64) -> String {
        format!("{}/{}.{copy}", self.dir, self.name)
    }

    /// Read both copies and keep the newest valid one.
    ///
    /// Returns `None` as the payload when neither copy exists.
    ///
    /// # Errors
    ///
    /// [`JournalError::MetadataCorrupt`] when a copy exists but none is
    /// valid; [`JournalError::Io`] if the namespace cannot be queried.
    pub async fn load<P: StorageProvider>(
        provider: &P,
        dir: &str,
        name: &'static str,
    ) -> Result<(Self, Option<Vec<u8>>), JournalError> {
        let mut this = Self {
            dir: dir.to_string(),
            name,
            generation: 0,
        };
        let mut found_any = false;
        let mut best: Option<(u64, Vec<u8>)> = None;
        for copy in 0..2 {
            let path = this.path(copy);
            if !provider.exists(&path).await? {
                continue;
            }
            found_any = true;
            // A copy that cannot be read is as good as a damaged one: the
            // other copy is what the two-copy scheme is for.
            let Ok(Some(candidate)) = read_copy(provider, &path).await else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(generation, _)| candidate.0 > *generation)
            {
                best = Some(candidate);
            }
        }
        match best {
            Some((generation, payload)) => {
                this.generation = generation;
                Ok((this, Some(payload)))
            }
            None if found_any => Err(JournalError::MetadataCorrupt { name }),
            None => Ok((this, None)),
        }
    }

    /// Durably replace the record with `payload`, in both copies.
    ///
    /// # Errors
    ///
    /// Any I/O error; at least one copy is still intact when one is returned.
    pub async fn store<P: StorageProvider>(
        &mut self,
        provider: &P,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        let generation = self.generation + 1;
        for copy in 0..2 {
            self.store_copy(provider, copy, generation, payload).await?;
        }
        self.generation = generation;
        Ok(())
    }

    /// Replace one copy through a temporary file, a sync, a rename, and a
    /// directory sync.
    async fn store_copy<P: StorageProvider>(
        &self,
        provider: &P,
        copy: u64,
        generation: u64,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        let target = self.path(copy);
        let temporary = format!("{target}.tmp");
        if provider.exists(&temporary).await? {
            provider.delete(&temporary).await?;
        }

        let file = provider
            .open(&temporary, OpenOptions::create_new_write().read(true))
            .await?;
        let blocks = BlockFile::new(file, BLOCK)?;
        let len = (HEADER + payload.len()).div_ceil(BLOCK);
        let mut buf = blocks.buffer(len)?;
        let bytes = buf.as_mut_slice();
        bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..16].copy_from_slice(&generation.to_le_bytes());
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| JournalError::InvalidConfig("metadata payload too large".into()))?;
        bytes[16..20].copy_from_slice(&payload_len.to_le_bytes());
        bytes[HEADER..HEADER + payload.len()].copy_from_slice(payload);
        let checksum = crc(bytes, payload);
        bytes[20..24].copy_from_slice(&checksum.to_le_bytes());
        blocks.write_blocks(0, buf.as_slice()).await?;
        blocks.sync().await?;
        drop(blocks);

        provider.rename(&temporary, &target).await?;
        provider.sync_dir(&self.dir).await?;
        Ok(())
    }
}

async fn read_copy<P: StorageProvider>(
    provider: &P,
    path: &str,
) -> Result<Option<(u64, Vec<u8>)>, JournalError> {
    let file = provider.open(path, OpenOptions::read_only()).await?;
    let size = file.size().await?;
    if size == 0 || !size.is_multiple_of(BLOCK_U64) {
        return Ok(None);
    }
    let blocks = BlockFile::new(file, BLOCK)?;
    let count = usize::try_from(size / BLOCK_U64)
        .map_err(|_| JournalError::InvalidConfig("metadata file too large".into()))?;
    let mut buf = blocks.buffer(count)?;
    blocks.read_blocks(0, buf.as_mut_slice()).await?;
    Ok(decode(buf.as_slice()))
}
