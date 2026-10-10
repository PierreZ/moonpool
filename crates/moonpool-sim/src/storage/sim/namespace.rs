//! Namespace resolution: path lookup and what a crash does to unsynced directory entries.

use std::{collections::BTreeMap, net::IpAddr};

use super::state::Name;

use super::FileId;
use super::engine::{StorageEngine, parent_directory};
use crate::{
    assert_reachable,
    sim::rng::sim_random,
    storage::{StorageError, StorageFaultKind, StorageFaultRecord},
};

/// The complete type at one namespace name. A crash resolves a name as one
/// entry, so a file and directory can never both occupy it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceEntry {
    Missing,
    File(FileId),
    Directory,
}

impl StorageEngine {
    /// Resolve a path against one process's virtual filesystem. The relative
    /// root (`.`) and absolute root (`/`) are separate and have no host path.
    /// Intermediate components are checked before `..` is folded, so a name
    /// cannot skip a missing directory or pass through a regular file.
    pub(super) fn resolve_path(
        &self,
        owner_ip: IpAddr,
        raw: &str,
        create_dirs: bool,
    ) -> Result<(String, Vec<String>), StorageError> {
        if raw.contains('\0') {
            return Err(StorageError::InvalidPath {
                path: raw.to_string(),
            });
        }
        if raw.is_empty() {
            if create_dirs {
                return Ok((".".to_string(), Vec::new()));
            }
            return Err(StorageError::NotFound {
                path: raw.to_string(),
            });
        }
        let mut current = if raw.starts_with('/') {
            "/".to_string()
        } else {
            ".".to_string()
        };
        let components: Vec<&str> = raw.split('/').filter(|part| !part.is_empty()).collect();
        let mut missing = Vec::new();
        for (index, component) in components.iter().enumerate() {
            let last = index + 1 == components.len();
            match *component {
                "." => {
                    if last && !create_dirs {
                        self.require_directory(owner_ip, &current)?;
                    }
                }
                ".." => {
                    if current == "." {
                        return Err(StorageError::NotFound {
                            path: raw.to_string(),
                        });
                    }
                    if current != "/" {
                        current = parent_directory(&current).to_string();
                    }
                }
                name => {
                    let next = if current == "." {
                        name.to_string()
                    } else if current == "/" {
                        format!("/{name}")
                    } else {
                        format!("{current}/{name}")
                    };
                    if create_dirs {
                        if self
                            .state
                            .path_to_file
                            .contains_key(&(owner_ip, next.clone()))
                        {
                            return Err(if last {
                                StorageError::AlreadyExists { path: next }
                            } else {
                                StorageError::NotADirectory { path: next }
                            });
                        }
                        if !self.is_directory(owner_ip, &next) && !missing.contains(&next) {
                            missing.push(next.clone());
                        }
                    } else if !last {
                        self.require_directory(owner_ip, &next)?;
                    }
                    current = next;
                }
            }
        }
        if raw.ends_with('/') && !create_dirs {
            self.require_directory(owner_ip, &current)?;
        }
        Ok((current, missing))
    }

    /// Resolve the namespace a crash leaves behind.
    ///
    /// Every divergence between the visible namespace and the durable one is
    /// an unsynced directory operation, and each is resolved independently: a
    /// name created since the last directory sync may not be there, and a name
    /// deleted or renamed away since then may still be. Contents are dropped
    /// once no surviving name reaches them.
    ///
    /// The coin is drawn only while `unsynced_dir_entry_loss_probability` is
    /// positive, so a configuration with the family off consumes no
    /// randomness — and then the visible namespace survives whole, which is
    /// what every test that does not care about entry durability expects.
    pub(super) fn resolve_namespace_crash(&mut self, ip: IpAddr) {
        let probability = self
            .state
            .config_for(ip)
            .unsynced_dir_entry_loss_probability;
        if probability > 0.0 {
            self.resolve_unsynced_entries(ip, probability);
        }

        self.prune_entries_without_parents(ip);

        self.refresh_file_paths(ip);

        // Whatever survived is what is on the disk now — for this process's
        // files only. Another process's unsynced entries are not made durable
        // by this crash.
        let mine = self.files_owned_by(ip);
        self.state
            .durable_paths
            .retain(|_, file_id| !mine.contains(file_id));
        let surviving: Vec<(Name, FileId)> = self
            .state
            .path_to_file
            .iter()
            .filter(|(_, file_id)| mine.contains(file_id))
            .map(|(name, file_id)| (name.clone(), *file_id))
            .collect();
        for (name, file_id) in surviving {
            self.state.durable_paths.insert(name, file_id);
        }
        self.state
            .durable_directories
            .retain(|(owner, _)| *owner != ip);
        self.state.durable_directories.extend(
            self.state
                .directories
                .iter()
                .filter(|(owner, _)| *owner == ip)
                .cloned(),
        );
    }

    /// Resolve each unsynced name as one complete entry type. A file replaced
    /// by a directory can roll back to the file or keep the directory, but a
    /// crash cannot leave both at that name.
    fn resolve_unsynced_entries(&mut self, ip: IpAddr, probability: f64) {
        let mut names: Vec<Name> = self
            .state
            .directories
            .iter()
            .chain(self.state.durable_directories.iter())
            .chain(self.state.path_to_file.keys())
            .chain(self.state.durable_paths.keys())
            .filter(|(owner, _)| *owner == ip)
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let visible = self.namespace_entry(&name, false);
            let durable = self.namespace_entry(&name, true);
            if visible == durable || sim_random::<f64>() >= probability {
                continue;
            }
            assert_reachable!("disk: crash lost an unsynced directory entry");
            self.state.record_fault(StorageFaultRecord {
                path: name.1.clone(),
                kind: StorageFaultKind::DirEntryLost,
                sectors: None,
            });
            self.state.path_to_file.remove(&name);
            self.state.directories.remove(&name);
            match durable {
                NamespaceEntry::File(file_id) => {
                    self.state.path_to_file.insert(name, file_id);
                }
                NamespaceEntry::Directory => {
                    self.state.directories.insert(name);
                }
                NamespaceEntry::Missing => {}
            }
        }
    }

    fn namespace_entry(&self, name: &Name, durable: bool) -> NamespaceEntry {
        let (files, directories) = if durable {
            (&self.state.durable_paths, &self.state.durable_directories)
        } else {
            (&self.state.path_to_file, &self.state.directories)
        };
        if let Some(file_id) = files.get(name) {
            NamespaceEntry::File(*file_id)
        } else if directories.contains(name) {
            NamespaceEntry::Directory
        } else {
            NamespaceEntry::Missing
        }
    }

    /// A synced child entry cannot survive when its parent directory's own
    /// name was lost. Prune directories from the top down, then file names.
    fn prune_entries_without_parents(&mut self, ip: IpAddr) {
        let names: Vec<Name> = self
            .state
            .directories
            .iter()
            .filter(|(owner, _)| *owner == ip)
            .cloned()
            .collect();
        for name in names {
            if !self.is_directory(ip, parent_directory(&name.1)) {
                self.state.directories.remove(&name);
            }
        }
        let orphaned: Vec<Name> = self
            .state
            .path_to_file
            .keys()
            .filter(|(owner, path)| *owner == ip && !self.is_directory(ip, parent_directory(path)))
            .cloned()
            .collect();
        for name in orphaned {
            self.state.path_to_file.remove(&name);
        }
    }

    /// Give each surviving image a stable fault coordinate after namespace
    /// rollback. Rare per-name outcomes can leave old and new rename aliases
    /// pointing to one image; the first surviving name in lexical order wins.
    /// An image with no surviving name keeps its last coordinate until the
    /// crash oracle has examined its bytes and it is discarded.
    fn refresh_file_paths(&mut self, ip: IpAddr) {
        let mut canonical = BTreeMap::<FileId, String>::new();
        for ((owner, path), file_id) in &self.state.path_to_file {
            if *owner == ip {
                canonical.entry(*file_id).or_insert_with(|| path.clone());
            }
        }
        for (file_id, path) in canonical {
            if let Some(file) = self.state.files.get_mut(&file_id) {
                file.path = path;
            }
        }
    }

    /// Forget this process's images after the byte-crash oracle has checked
    /// even those whose last name was lost in the namespace crash.
    pub(super) fn collect_unreachable_files(&mut self, ip: IpAddr) {
        // Contents no surviving name reaches are gone with the name.
        let linked: Vec<FileId> = self
            .state
            .path_to_file
            .values()
            .chain(self.state.durable_paths.values())
            .copied()
            .collect();
        self.state
            .files
            .retain(|file_id, file| file.owner_ip != ip || linked.contains(file_id));
    }
}
