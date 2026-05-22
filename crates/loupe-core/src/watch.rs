//! Filesystem watcher that maps `notify` events to DICE key updates.
//!
//! Foundation for `loupe-watch`: keeps a long-lived `Dice` instance
//! warm and invalidates only the touched `FileContentKey` /
//! `DirListingKey` entries on each filesystem event, instead of the
//! v1 "bump the version on every invocation" trick that recomputes
//! everything.
//!
//! Scope of this PR: utility module + tests. Engine integration
//! (`EngineBuilder::with_file_watcher(true)` + long-lived `Dice` reuse
//! across analyze calls) is a follow-on PR — the current `Engine`
//! creates a fresh `Dice` per instance which would defeat the whole
//! point of a watcher.
//!
//! ## Usage shape
//!
//! ```no_run
//! # use loupe_core::watch::WorkspaceWatcher;
//! # async fn demo(dice: std::sync::Arc<dice::Dice>) -> anyhow::Result<()> {
//! let mut watcher = WorkspaceWatcher::new(dice, std::path::PathBuf::from("/work"))?;
//! watcher.poll_and_apply().await?; // drains any pending fs events
//! # Ok(()) }
//! ```
//!
//! The watcher batches events and applies them in one `DiceTransactionUpdater::commit`
//! so a flurry of editor saves (multi-file rename, IDE format-on-save)
//! invalidates the affected keys in a single DICE version bump.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Context, Result};
use dice::Dice;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::dice_keys::{DirListingKey, FileContentKey, SharedResult};

/// `notify`-backed watcher that keeps a `Dice` cache consistent with
/// on-disk changes by re-computing the relevant `FileContentKey` /
/// `DirListingKey` entries.
pub struct WorkspaceWatcher {
    dice: Arc<Dice>,
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    root: PathBuf,
}

impl WorkspaceWatcher {
    /// Start watching `root` recursively. The watcher itself is
    /// `Send + Sync`; events drain into a synchronous `mpsc::Receiver`
    /// that the caller pumps via [`Self::poll_and_apply`].
    pub fn new(dice: Arc<Dice>, root: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("creating notify watcher")?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
        Ok(Self {
            dice,
            _watcher: watcher,
            rx,
            root,
        })
    }

    /// Workspace root the watcher is rooted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Drain every pending filesystem event, translate to DICE key
    /// invalidations, and commit them in one transaction. Returns the
    /// number of distinct keys that were invalidated.
    ///
    /// Idempotent on empty input — a no-op transaction commit is cheap.
    pub async fn poll_and_apply(&mut self) -> Result<usize> {
        let mut file_changes: BTreeSet<PathBuf> = BTreeSet::new();
        let mut dir_changes: BTreeSet<PathBuf> = BTreeSet::new();
        loop {
            match self.rx.try_recv() {
                Ok(Ok(event)) => classify(event, &mut file_changes, &mut dir_changes),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "notify error");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("watcher channel disconnected");
                }
            }
        }
        let total = file_changes.len() + dir_changes.len();
        if total == 0 {
            return Ok(0);
        }

        // Re-read the affected leaf inputs synchronously, then push
        // every (key, new_value) into one updater + commit.
        let mut updater = self.dice.updater();
        for path in file_changes {
            let bytes = std::fs::read(&path).ok();
            updater
                .changed_to(vec![(
                    FileContentKey(path.clone()),
                    match bytes {
                        Some(b) => SharedResult::ok(b),
                        None => {
                            SharedResult::err(format!("file no longer present: {}", path.display()))
                        }
                    },
                )])
                .with_context(|| format!("updating FileContentKey({})", path.display()))?;
        }
        for path in dir_changes {
            let listing = read_dir_meta(&path);
            updater
                .changed_to(vec![(
                    DirListingKey(path.clone()),
                    match listing {
                        Some(v) => SharedResult::ok(v),
                        None => {
                            SharedResult::err(format!("dir no longer present: {}", path.display()))
                        }
                    },
                )])
                .with_context(|| format!("updating DirListingKey({})", path.display()))?;
        }
        let _ = updater.commit().await;
        Ok(total)
    }
}

fn classify(
    event: Event,
    file_changes: &mut BTreeSet<PathBuf>,
    dir_changes: &mut BTreeSet<PathBuf>,
) {
    use EventKind::*;
    match event.kind {
        // Content edits: invalidate the file's content key. Parent
        // directory listing is unchanged (file's name didn't appear or
        // disappear).
        Modify(_) => {
            for p in event.paths {
                if p.is_file() {
                    file_changes.insert(p);
                }
            }
        }
        // Create / remove: invalidate the parent directory listing
        // (entry name set changed) AND the file's content key (in case
        // it was being read; harmless if not).
        Create(_) | Remove(_) => {
            for p in event.paths {
                if let Some(parent) = p.parent() {
                    dir_changes.insert(parent.to_path_buf());
                }
                file_changes.insert(p);
            }
        }
        // Access events don't change content or listing — ignore.
        Access(_) => {}
        // Renames / moves: parent listing changes (entry name set),
        // both the source and destination paths' content keys may need
        // to invalidate too.
        Any | Other => {
            for p in event.paths {
                if let Some(parent) = p.parent() {
                    dir_changes.insert(parent.to_path_buf());
                }
                file_changes.insert(p);
            }
        }
    }
}

fn read_dir_meta(path: &Path) -> Option<Vec<crate::dice_keys::DirEntryMeta>> {
    use crate::dice_keys::{DirEntryKind, DirEntryMeta};
    let entries = std::fs::read_dir(path).ok()?;
    let mut out: Vec<DirEntryMeta> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match entry.file_type() {
            Ok(ft) if ft.is_file() => DirEntryKind::File,
            Ok(ft) if ft.is_dir() => DirEntryKind::Dir,
            Ok(ft) if ft.is_symlink() => DirEntryKind::Symlink,
            Ok(_) => DirEntryKind::Other,
            Err(_) => DirEntryKind::Other,
        };
        out.push(DirEntryMeta { name, kind });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_modify_records_file_change_only() {
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("foo.bzl");
        std::fs::write(&path, "x").unwrap();
        classify(
            Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Content,
                )),
                paths: vec![path.clone()],
                attrs: Default::default(),
            },
            &mut files,
            &mut dirs,
        );
        assert_eq!(files.len(), 1);
        assert_eq!(dirs.len(), 0);
    }

    #[test]
    fn classify_create_records_dir_and_file() {
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new.bzl");
        classify(
            Event {
                kind: EventKind::Create(notify::event::CreateKind::File),
                paths: vec![path.clone()],
                attrs: Default::default(),
            },
            &mut files,
            &mut dirs,
        );
        assert_eq!(files.len(), 1);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs.iter().next().unwrap(), tmp.path());
    }

    #[test]
    fn read_dir_meta_sorts_and_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("c")).unwrap();
        let entries = read_dir_meta(tmp.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c"]);
        assert_eq!(entries[0].kind, crate::dice_keys::DirEntryKind::File);
        assert_eq!(entries[2].kind, crate::dice_keys::DirEntryKind::Dir);
    }
}
