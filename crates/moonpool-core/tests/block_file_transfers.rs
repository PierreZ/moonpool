//! `BlockFile` against a deliberately awkward `StorageFile`.
//!
//! `StorageFile` may move fewer bytes than asked for, and a file with
//! direct-I/O constraints may refuse any request that is misaligned. Those two
//! rules interact: the obvious continuation after a short transfer — resume at
//! `offset + moved`, with the rest of the buffer — is exactly what the file
//! then refuses.
//!
//! The fake here holds both rules at once. Its three alignments differ, so no
//! single number stands in for all of them, and a naive continuation violates
//! at least one; and it refuses every misaligned request outright instead of
//! quietly serving it, so a `BlockFile` that issued one would fail rather than
//! pass by luck.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::{AlignedBuf, BlockFile, IoConstraints, StorageFile};

/// Offsets must be 4 KiB aligned...
const OFFSET_ALIGNMENT: u64 = 4096;
/// ...lengths only 512-byte aligned...
const LENGTH_ALIGNMENT: usize = 512;
/// ...and buffers only 512-byte aligned. A 512-byte short transfer is
/// therefore perfectly legal for this file, and resuming from it would
/// violate the offset alignment.
const MEMORY_ALIGNMENT: usize = 512;

/// The awkward short count the first transfer returns.
const SHORT_COUNT: usize = 512;

/// One request the fake was asked to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Request {
    offset: u64,
    len: usize,
    /// Whether every one of the three alignments was satisfied.
    aligned: bool,
}

#[derive(Debug, Default)]
struct FakeState {
    bytes: Vec<u8>,
    requests: Vec<Request>,
    /// Transfers served so far; the first one is short.
    served: usize,
}

/// A `StorageFile` with non-trivial constraints and an awkward first transfer.
#[derive(Debug, Clone)]
struct AwkwardFile {
    state: Arc<Mutex<FakeState>>,
}

impl AwkwardFile {
    fn new(size: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                bytes: vec![0; size],
                ..FakeState::default()
            })),
        }
    }

    fn constraints() -> IoConstraints {
        IoConstraints::new(OFFSET_ALIGNMENT, LENGTH_ALIGNMENT, MEMORY_ALIGNMENT)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior test panicked")
    }

    /// Record a request, refuse it if misaligned, and decide how many bytes it
    /// moves: the first transfer is short, every later one is complete.
    fn admit(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let aligned = Self::constraints().check(offset, buf).is_ok();
        let mut state = self.state();
        state.requests.push(Request {
            offset,
            len: buf.len(),
            aligned,
        });
        if !aligned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "misaligned request refused by the device",
            ));
        }
        state.served += 1;
        Ok(if state.served == 1 {
            SHORT_COUNT.min(buf.len())
        } else {
            buf.len()
        })
    }
}

impl StorageFile for AwkwardFile {
    fn constraints(&self) -> IoConstraints {
        Self::constraints()
    }

    fn is_direct_io(&self) -> bool {
        true
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let moved = self.admit(offset, buf)?;
        let start = usize::try_from(offset).expect("offset fits in usize");
        let state = self.state();
        let available = state.bytes.len().saturating_sub(start).min(moved);
        buf[..available].copy_from_slice(&state.bytes[start..start + available]);
        Ok(available)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let moved = self.admit(offset, buf)?;
        let start = usize::try_from(offset).expect("offset fits in usize");
        let mut state = self.state();
        if start + moved > state.bytes.len() {
            state.bytes.resize(start + moved, 0);
        }
        state.bytes[start..start + moved].copy_from_slice(&buf[..moved]);
        Ok(moved)
    }

    async fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    async fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }

    async fn size(&self) -> io::Result<u64> {
        Ok(self.state().bytes.len() as u64)
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        let size = usize::try_from(size).expect("size fits in usize");
        self.state().bytes.resize(size, 0);
        Ok(())
    }
}

/// Stream I/O is not available on a file with direct-I/O constraints; the
/// block layer never reaches for it.
impl AsyncRead for AwkwardFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::Unsupported)))
    }
}

impl AsyncWrite for AwkwardFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::Unsupported)))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for AwkwardFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _pos: io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::Unsupported)))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime")
}

/// 8192 asked for, 512 returned, and the continuation must still be legal.
///
/// Naively resuming at offset 512 would break the 4 KiB offset alignment; the
/// fake would refuse it and the write would fail. `BlockFile` instead resumes
/// from the boundary below the frontier and rewrites those 512 bytes.
#[test]
fn a_short_write_is_completed_without_an_invalid_request() {
    runtime().block_on(async {
        let file = AwkwardFile::new(0);
        let recorder = file.clone();
        let blocks = BlockFile::new(file, 8192).expect("8192 is a multiple of every alignment");

        let mut page = blocks.buffer(1);
        for (index, byte) in page.as_mut_slice().iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).expect("modulo fits in u8");
        }
        blocks
            .write_blocks(0, page.as_slice())
            .await
            .expect("the short write must be completed, not failed");

        let state = recorder.state();
        assert!(
            state.requests.iter().all(|request| request.aligned),
            "every request must satisfy all three alignments: {:?}",
            state.requests
        );
        assert_eq!(
            state.requests.len(),
            2,
            "one short transfer, then one that completes it: {:?}",
            state.requests
        );
        assert_eq!(
            state.requests[1].offset, 0,
            "the retry resumes from the aligned boundary below the frontier"
        );
        assert_eq!(state.bytes, page.as_slice(), "every byte must have landed");
    });
}

/// The same for reads, and the bytes must be the file's, not a partially
/// filled buffer.
#[test]
fn a_short_read_is_completed_without_an_invalid_request() {
    runtime().block_on(async {
        let expected: Vec<u8> = (0..8192)
            .map(|index| u8::try_from(index % 251).expect("modulo fits in u8"))
            .collect();
        let file = AwkwardFile::new(0);
        file.state().bytes = expected.clone();
        let recorder = file.clone();
        let blocks = BlockFile::new(file, 8192).expect("wrap failed");

        let mut page = blocks.buffer(1);
        blocks
            .read_blocks(0, page.as_mut_slice())
            .await
            .expect("the short read must be completed, not failed");

        assert_eq!(page.as_slice(), expected, "every byte must have arrived");
        let state = recorder.state();
        assert!(
            state.requests.iter().all(|request| request.aligned),
            "every request must satisfy all three alignments: {:?}",
            state.requests
        );
        assert_eq!(state.requests.len(), 2, "{:?}", state.requests);
    });
}

/// A transfer that starts at a later block still resumes on a boundary the
/// file accepts — the frontier is relative to the transfer, the alignment is
/// absolute.
#[test]
fn a_short_transfer_mid_file_resumes_on_an_absolute_boundary() {
    runtime().block_on(async {
        let file = AwkwardFile::new(0);
        let recorder = file.clone();
        let blocks = BlockFile::new(file, 8192).expect("wrap failed");
        blocks.grow_to_blocks(4).await.expect("grow failed");

        let mut page = blocks.buffer(2);
        page.as_mut_slice().fill(0x5C);
        blocks
            .write_blocks(2, page.as_slice())
            .await
            .expect("write failed");

        let state = recorder.state();
        assert!(state.requests.iter().all(|request| request.aligned));
        assert_eq!(
            state.requests[0].offset,
            2 * 8192,
            "the first request starts at the block's own offset"
        );
        assert_eq!(
            state.requests[1].offset,
            2 * 8192,
            "and the retry resumes from the same aligned boundary"
        );
    });
}

/// A caller's own buffer must satisfy the file's memory alignment, and is
/// told so before any request is issued rather than by the device afterwards.
#[test]
fn a_misaligned_caller_buffer_is_refused_up_front() {
    runtime().block_on(async {
        let file = AwkwardFile::new(8192);
        let recorder = file.clone();
        let blocks = BlockFile::new(file, 8192).expect("wrap failed");

        // Aligned length, deliberately misaligned address.
        let backing = AlignedBuf::zeroed(8192 + MEMORY_ALIGNMENT, MEMORY_ALIGNMENT);
        let misaligned = &backing.as_slice()[1..8193];
        let error = blocks
            .write_blocks(0, misaligned)
            .await
            .expect_err("a misaligned buffer must be refused");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            recorder.state().requests.is_empty(),
            "the file must never see the request"
        );
    });
}
