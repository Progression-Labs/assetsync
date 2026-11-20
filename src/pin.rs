//! Pin: hash a set of files under a root, put every blob in the local cache, upload what the
//! origin lacks, and (last, so a lock never names what the origin has not got) rewrite the lock.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::lock::{Entry, Lock, is_safe_path};
use crate::origin::Origin;
use crate::par::{Progress, local_workers, mb, par};
use crate::store::{self, Store};

/// Parallel uploads.
pub const UPLOAD_WORKERS: usize = 12;

pub struct PinOptions<'a> {
    pub store: &'a Store,
    /// Where blobs are uploaded; `None` pins into the cache and the lock only.
    pub origin: Option<&'a dyn Origin>,
    /// The tree the lock's paths are relative to.
    pub root: PathBuf,
    /// Files or directories under `root`, relative to it, to pin (a directory is walked; symlinks
    /// are skipped). Empty pins the whole of `root`.
    pub paths: Vec<String>,
    /// Keep the old lock's entries outside `paths` (adding to a lock, like zero's `raw.lock`).
    /// Without it the lock is exactly what `paths` holds now.
    pub merge: bool,
    /// The lock's `# key value` header; `None` keeps the old lock's.
    pub header: Option<Vec<(String, String)>>,
    /// Parallel uploads ([`UPLOAD_WORKERS`] when 0).
    pub workers: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinReport {
    /// Entries in the written lock, distinct blobs among them, and their bytes.
    pub files: usize,
    pub blobs: usize,
    pub bytes: u64,
    /// Files hashed this run (a file already linked to its blob is not read again).
    pub hashed: usize,
    /// Blobs uploaded to the origin, and their bytes.
    pub uploaded: usize,
    pub uploaded_bytes: u64,
    /// Blobs the lock names that neither the origin nor this cache has (kept entries of a merge
    /// pinned on another machine): their sha256s.
    pub unavailable: Vec<String>,
}

/// What this crate leaves in a tree, never pinned: sync stamps and link temporaries.
fn ours(name: &str) -> bool {
    name.starts_with(".assetsync-") || name.contains(store::LINK_TMP)
}

/// Every regular file under `root/rel` (a file or a directory), relative to `root`, skipping
/// `skip` and this crate's own files. Symlinks are not followed: a tree is files and directories.
fn walk(root: &Path, rel: &str, skip: Option<&Path>, out: &mut Vec<String>) -> Result<()> {
    let mut stack = vec![rel.trim_end_matches('/').to_string()];
    while let Some(rel) = stack.pop() {
        let abs = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel)
        };
        let meta = fs::symlink_metadata(&abs).with_context(|| format!("{rel}: not there"))?;
        if meta.is_dir() {
            for entry in fs::read_dir(&abs).with_context(|| abs.display().to_string())? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    bail!("{}: a name that is not UTF-8", entry.path().display());
                };
                if ours(name) {
                    continue;
                }
                stack.push(if rel.is_empty() {
                    name.to_string()
                } else {
                    format!("{rel}/{name}")
                });
            }
        } else if meta.is_file() && skip != Some(abs.as_path()) {
            if rel.contains('\n') {
                bail!("{rel}: a path with a newline cannot go in a lock");
            }
            out.push(rel);
        }
    }
    Ok(())
}

fn under(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || path == prefix
        || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// Pins into the lock at `lock_path`.
pub fn pin(lock_path: &Path, opts: &PinOptions) -> Result<PinReport> {
    let store = opts.store;
    let log = &store.log;
    let root = &opts.root;
    for p in &opts.paths {
        if !is_safe_path(p.trim_end_matches('/')) && !p.is_empty() {
            bail!("{p}: pin takes paths relative to the root, inside it");
        }
    }
    let old_lock = if lock_path.exists() {
        Some(Lock::read(lock_path)?)
    } else {
        None
    };
    let old: HashMap<&str, &str> = old_lock
        .iter()
        .flat_map(|l| l.entries.iter().map(|e| (e.path.as_str(), e.sha.as_str())))
        .collect();

    let mut paths = Vec::new();
    let lock_abs = fs::canonicalize(lock_path).ok();
    let root_abs = fs::canonicalize(root).with_context(|| root.display().to_string())?;
    let skip = lock_abs.as_deref().and_then(|l| {
        l.strip_prefix(&root_abs).ok().map(|rel| root.join(rel))
    });
    let roots: Vec<String> = if opts.paths.is_empty() {
        vec![String::new()]
    } else {
        opts.paths.clone()
    };
    for p in &roots {
        walk(root, p, skip.as_deref(), &mut paths)?;
    }
    paths.sort();
    paths.dedup();
    log(&format!("pinning {} files", paths.len()));

    // Hash (a file already linked to its blob in the cache is that blob: no read) and ingest.
    let progress = Progress::new(log, "hashed", paths.len());
    let hashed = std::sync::atomic::AtomicUsize::new(0);
    let mut entries = par(&paths, local_workers(), |rel| {
        let file = root.join(rel);
        let meta = fs::metadata(&file).with_context(|| format!("{rel}: missing"))?;
        let linked = old.get(rel.as_str()).filter(|sha| {
            fs::metadata(store.blob(sha))
                .is_ok_and(|b| b.ino() == meta.ino() && b.dev() == meta.dev())
        });
        let (sha, size) = match linked {
            Some(sha) => (sha.to_string(), meta.len()),
            None => {
                hashed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                store::hash_file(&file).with_context(|| rel.clone())?
            }
        };
        store.ingest(&file, &sha)?;
        progress.tick();
        Ok(Entry {
            sha,
            size,
            path: rel.clone(),
        })
    })?;
    if opts.merge
        && let Some(l) = &old_lock
    {
        // What the lock already lists outside what was named keeps its entry (and its blob):
        // another machine may have pinned it, and this one may not have the file.
        entries.extend(
            l.entries
                .iter()
                .filter(|e| !roots.iter().any(|p| under(&e.path, p)))
                .cloned(),
        );
    }
    let header = match &opts.header {
        Some(h) => h.clone(),
        None => old_lock.map(|l| l.header).unwrap_or_default(),
    };
    let mut lock = Lock { header, entries };
    lock.sort();
    let blobs = lock.blobs();
    let mut report = PinReport {
        files: lock.entries.len(),
        blobs: blobs.len(),
        bytes: blobs.iter().map(|b| b.1).sum(),
        hashed: hashed.into_inner(),
        ..PinReport::default()
    };
    log(&format!(
        "{} files, {} blobs, {}",
        report.files,
        report.blobs,
        mb(report.bytes)
    ));

    if let Some(origin) = opts.origin {
        if !origin.writable() {
            bail!("the origin is read-only: pin needs one that takes uploads");
        }
        let have = origin.list()?;
        let mut missing: Vec<_> = blobs
            .iter()
            .filter(|(sha, ..)| !have.contains_key(*sha))
            .collect();
        missing.retain(|(sha, ..)| {
            let here = store.has(sha);
            if !here {
                log(&format!("{sha} is in the lock and not at the origin, and this cache lacks it: pin it from the machine that has the file"));
                report.unavailable.push(sha.to_string());
            }
            here
        });
        missing.sort_by_key(|b| std::cmp::Reverse(b.1));
        report.uploaded = missing.len();
        report.uploaded_bytes = missing.iter().map(|b| b.1).sum();
        log(&format!(
            "origin has {}, uploading {} blobs ({})",
            have.len(),
            missing.len(),
            mb(report.uploaded_bytes)
        ));
        let progress = Progress::new(log, "uploaded", missing.len());
        let workers = if opts.workers == 0 {
            UPLOAD_WORKERS
        } else {
            opts.workers
        };
        par(&missing, workers, |(sha, size, _)| {
            origin.put(sha, &store.blob(sha), *size)?;
            progress.tick();
            Ok(())
        })?;
    }

    store.record_use(blobs.iter().map(|(sha, _, path)| (*sha, *path)))?;
    lock.write(lock_path)?;
    log(&format!("wrote {}", lock_path.display()));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::store_in;

    fn opts<'a>(store: &'a Store, root: &Path) -> PinOptions<'a> {
        PinOptions {
            store,
            origin: None,
            root: root.to_path_buf(),
            paths: vec![],
            merge: false,
            header: None,
            workers: 0,
        }
    }

    #[test]
    fn pin_locks_a_tree_and_skips_its_own_files() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let tree = d.path().join("tree");
        fs::create_dir_all(tree.join("voice/a")).unwrap();
        fs::write(tree.join("voice/a/one.mp3"), b"one").unwrap();
        fs::write(tree.join("voice/two words.mp3"), b"two").unwrap();
        fs::write(tree.join(".assetsync-voice.lock"), b"stamp").unwrap();
        std::os::unix::fs::symlink("voice/a/one.mp3", tree.join("link.mp3")).unwrap();
        let lock_path = tree.join("voice.lock");
        fs::write(&lock_path, "").unwrap();
        let mut o = opts(&store, &tree);
        o.header = Some(vec![("source".into(), "test".into())]);
        let r = pin(&lock_path, &o).unwrap();
        assert_eq!((r.files, r.blobs, r.hashed), (2, 2, 2));
        let lock = Lock::read(&lock_path).unwrap();
        let paths: Vec<_> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["voice/a/one.mp3", "voice/two words.mp3"]);
        assert_eq!(lock.header("source"), Some("test"));
        // The tree's files are the blobs now; a second pin reads nothing and keeps the header.
        o.header = None;
        let r = pin(&lock_path, &o).unwrap();
        assert_eq!(r.hashed, 0);
        assert_eq!(Lock::read(&lock_path).unwrap(), lock);
    }

    #[test]
    fn a_merge_keeps_what_was_not_named_and_replaces_what_was() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let root = d.path().join("root");
        fs::create_dir_all(root.join("raw/a")).unwrap();
        fs::write(root.join("raw/a/x.zip"), b"x").unwrap();
        fs::write(root.join("raw/b.zip"), b"b").unwrap();
        let lock_path = d.path().join("raw.lock");
        let mut o = opts(&store, &root);
        o.merge = true;
        o.paths = vec!["raw/a/".into(), "raw/b.zip".into()];
        pin(&lock_path, &o).unwrap();
        // Another machine's file, and raw/a re-pinned without x.zip but with y.zip.
        fs::remove_file(root.join("raw/a/x.zip")).unwrap();
        fs::write(root.join("raw/a/y.zip"), b"y").unwrap();
        o.paths = vec!["raw/a".into()];
        pin(&lock_path, &o).unwrap();
        let paths: Vec<_> = Lock::read(&lock_path)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, ["raw/a/y.zip", "raw/b.zip"]);
        o.paths = vec!["../out".into()];
        assert!(pin(&lock_path, &o).is_err());
    }

    #[test]
    fn under_is_by_component() {
        assert!(under("raw/a/x", "raw/a"));
        assert!(under("raw/a/x", "raw/a/"));
        assert!(under("raw/a", "raw/a"));
        assert!(!under("raw/ab", "raw/a"));
        assert!(under("anything", ""));
    }
}
