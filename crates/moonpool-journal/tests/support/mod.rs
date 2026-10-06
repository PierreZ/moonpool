//! A storage provider that fails exactly where a test says, wrapped around
//! the simulator's: the faults a crash loop reaches only by chance, placed on
//! one chosen operation.

use std::io;
use std::sync::{Arc, Mutex};

use moonpool_core::{OpenOptions, StorageProvider};

/// What the wrapper still has to do; each fault fires once.
#[derive(Debug, Default)]
struct Plan {
    /// After a rename onto a path ending with this, fail the next
    /// directory sync.
    sync_dir_fails_after_rename_to: Option<String>,
    /// The next directory sync fails.
    sync_dir_fails: bool,
    /// Refuse a rename onto a path ending with this — the process dies just
    /// before it.
    rename_refused_to: Option<String>,
}

/// The faults a test arms for a [`Scripted`] provider.
#[derive(Debug, Clone, Default)]
pub struct Script(Arc<Mutex<Plan>>);

impl Script {
    fn with<T>(&self, f: impl FnOnce(&mut Plan) -> T) -> T {
        f(&mut self.0.lock().expect("script lock poisoned"))
    }

    /// The directory sync right after the next rename onto `suffix` fails:
    /// the renamed file is there, but its name is not durable.
    pub fn fail_sync_dir_after_rename_to(&self, suffix: &str) {
        self.with(|plan| plan.sync_dir_fails_after_rename_to = Some(suffix.to_string()));
    }

    /// The next rename onto `suffix` never happens: the writer stops there,
    /// as a crash would stop it.
    pub fn refuse_rename_to(&self, suffix: &str) {
        self.with(|plan| plan.rename_refused_to = Some(suffix.to_string()));
    }
}

/// `P` with a [`Script`] of faults.
#[derive(Debug, Clone)]
pub struct Scripted<P> {
    inner: P,
    script: Script,
}

impl<P> Scripted<P> {
    pub fn new(inner: P, script: Script) -> Self {
        Self { inner, script }
    }
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("{what} (scripted fault)"))
}

impl<P: StorageProvider> StorageProvider for Scripted<P> {
    type File = P::File;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        self.inner.open(path, options).await
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        self.inner.exists(path).await
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        self.inner.delete(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let refused = self.script.with(|plan| {
            let hit = plan
                .rename_refused_to
                .as_deref()
                .is_some_and(|suffix| to.ends_with(suffix));
            if hit {
                plan.rename_refused_to = None;
            }
            hit
        });
        if refused {
            return Err(injected("rename refused"));
        }
        self.inner.rename(from, to).await?;
        self.script.with(|plan| {
            if plan
                .sync_dir_fails_after_rename_to
                .as_deref()
                .is_some_and(|suffix| to.ends_with(suffix))
            {
                plan.sync_dir_fails_after_rename_to = None;
                plan.sync_dir_fails = true;
            }
        });
        Ok(())
    }

    async fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path).await
    }

    async fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        self.inner.list_dir(path).await
    }

    async fn sync_dir(&self, path: &str) -> io::Result<()> {
        if self
            .script
            .with(|plan| std::mem::take(&mut plan.sync_dir_fails))
        {
            return Err(injected("directory sync failed"));
        }
        self.inner.sync_dir(path).await
    }
}
