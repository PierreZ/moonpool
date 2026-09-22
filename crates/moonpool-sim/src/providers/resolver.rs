//! A deterministic, scripted [`Resolver`] for simulations.

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use moonpool_core::{Resolver, split_host_port};

/// What the table answers for one name.
#[derive(Debug, Clone)]
enum Answer {
    Addresses(Vec<IpAddr>),
    Fail(io::ErrorKind),
}

#[derive(Debug, Default)]
struct Table {
    names: BTreeMap<String, Answer>,
}

/// A [`Resolver`] that answers from a table the simulation edits while it runs.
///
/// Names map to IP lists; the port comes from the query, as with a real
/// lookup. A numeric target resolves to itself; an unknown name fails with
/// [`io::ErrorKind::NotFound`]; [`fail`](Self::fail) scripts any other lookup
/// error. Answers are immediate and draw no randomness, so a script that
/// repoints a name at a restarted or moved process replays identically for a
/// seed.
///
/// Clones share the table: publish one in the simulation state, hand clones
/// to processes and workloads, and edit it from a fault injector.
#[derive(Debug, Clone, Default)]
pub struct ScriptedResolver {
    table: Arc<Mutex<Table>>,
    lookups: Arc<AtomicU64>,
}

impl ScriptedResolver {
    /// An empty table: every name is unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// Point `name` at `addresses` (replacing any earlier answer).
    pub fn set(&self, name: &str, addresses: Vec<IpAddr>) {
        self.table()
            .names
            .insert(name.to_string(), Answer::Addresses(addresses));
    }

    /// Make lookups of `name` fail with `kind`.
    pub fn fail(&self, name: &str, kind: io::ErrorKind) {
        self.table()
            .names
            .insert(name.to_string(), Answer::Fail(kind));
    }

    /// Forget `name`: lookups fail with [`io::ErrorKind::NotFound`].
    pub fn remove(&self, name: &str) {
        self.table().names.remove(name);
    }

    /// Lookups answered so far (numeric targets included).
    #[must_use]
    pub fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    fn answer(&self, target: &str) -> io::Result<Vec<SocketAddr>> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        if let Ok(literal) = target.parse::<SocketAddr>() {
            return Ok(vec![literal]);
        }
        let (host, port) = split_host_port(target)?;
        let answer = self.table().names.get(host).cloned();
        match answer {
            Some(Answer::Addresses(addresses)) if !addresses.is_empty() => Ok(addresses
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect()),
            Some(Answer::Fail(kind)) => Err(io::Error::new(
                kind,
                format!("scripted lookup failure for {host}"),
            )),
            Some(Answer::Addresses(_)) | None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{host} has no scripted address"),
            )),
        }
    }
}

impl Resolver for ScriptedResolver {
    async fn resolve(&self, target: &str) -> io::Result<Vec<SocketAddr>> {
        self.answer(target)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr};

    use super::ScriptedResolver;

    #[test]
    fn answers_follow_the_script() {
        let resolver = ScriptedResolver::new();
        let clone = resolver.clone();
        assert_eq!(
            resolver.answer("10.0.1.1:4500").ok(),
            Some(vec!["10.0.1.1:4500".parse().expect("literal")])
        );
        assert_eq!(
            resolver.answer("db:4500").map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::NotFound)
        );
        clone.set("db", vec![IpAddr::V4(Ipv4Addr::new(10, 0, 1, 2))]);
        assert_eq!(
            resolver.answer("db:4500").ok(),
            Some(vec!["10.0.1.2:4500".parse().expect("literal")])
        );
        clone.fail("db", io::ErrorKind::TimedOut);
        assert_eq!(
            resolver.answer("db:4500").map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::TimedOut)
        );
        clone.remove("db");
        assert!(resolver.answer("db:4500").is_err());
        assert_eq!(resolver.lookups(), 5);
    }
}
