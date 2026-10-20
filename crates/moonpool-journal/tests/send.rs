//! The journal is written against the local provider traits, so nothing in
//! its signatures promises `Send`. Over a `Send` provider it must still be
//! `Send`, and so must every future its async methods return, or a Tokio
//! service could not hold it across an `.await` in a spawned task. These
//! assertions are checked when this file compiles; the test only runs them.

use moonpool_core::{TokioStorageProvider, TokioTaskProvider};
use moonpool_journal::{Batch, Journal, JournalConfig, JournalId, NoSpares, SpareSpawner, Spares};

type Plain = Journal<TokioStorageProvider>;
type Spared = Journal<TokioStorageProvider, Spares<TokioTaskProvider>>;

fn is_send<T: Send>(_: &T) {}

const fn send<T: Send>() {}

/// Build each future of `journal`'s async methods, and drop it unpolled.
fn method_futures_are_send<S: SpareSpawner<TokioStorageProvider>>(
    journal: &mut Journal<TokioStorageProvider, S>,
) where
    Journal<TokioStorageProvider, S>: Send + Sync,
{
    is_send(&journal.read(0));
    is_send(&journal.replay(..));
    is_send(&journal.commit(Batch::new()));
    is_send(&journal.prepare_spare());
}

#[test]
fn a_journal_over_tokio_is_send_with_or_without_spares() {
    send::<Plain>();
    send::<Spared>();
    send::<NoSpares>();
    send::<Spares<TokioTaskProvider>>();
}

#[test]
fn the_futures_of_a_journal_over_tokio_are_send() {
    let provider = TokioStorageProvider::new();
    let config = JournalConfig::default();
    is_send(&Plain::create(
        provider.clone(),
        "dir",
        JournalId(1),
        config,
        b"",
    ));
    is_send(&Plain::open(provider.clone(), "dir", JournalId(1), config));
    is_send(&Plain::peek_meta(&provider, "dir", JournalId(1)));
    let plain: fn(&mut Plain) = method_futures_are_send;
    let spared: fn(&mut Spared) = method_futures_are_send;
    let _ = (plain, spared);
}
