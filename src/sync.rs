//! Sync: fetch the blobs a lock names that the cache lacks (parallel, resumable, verified), then
//! link them into a destination tree. A whole-lock sync also removes what an earlier sync of the
//! same lock linked there that this lock no longer names; a partial one (`only`) just links.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::lock::Lock;
use crate::origin::Origin;
use crate::par::{Progress, local_workers, mb, par};
use crate::store::{self, LINK_TMP, Store};

/// Parallel downloads. Small files are latency-bound: zero measured 16 workers at 143 s, 32 at
/// 99 s and 64 at 78-89 s over the same 3,359 blobs on a ~10 MB/s link.
pub const FETCH_WORKERS: usize = 64;

/// How many times a blob is fetched when what arrives does not hash to the lock's sha256.
pub const FETCH_ATTEMPTS: u32 = 3;

pub struct SyncOptions<'a> {
    pub store: &'a Store,
    /// Where missing blobs come from; a sync whose cache has every blob needs none.
    pub origin: Option<&'a dyn Origin>,
    /// Only the entries under these path prefixes (`raw/Bistro/`); nothing is removed.
    pub only: Vec<String>,
    /// Parallel downloads ([`FETCH_WORKERS`] when 0).
    pub workers: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Entries the sync covered (after `only`).
    pub files: usize,
    /// Distinct blobs among them.
    pub blobs: usize,
    /// Blobs downloaded into the cache, and their bytes.
    pub fetched: usize,
    pub fetched_bytes: u64,
    /// Files (re)linked into the destination.
    pub linked: usize,
    /// Files already in place.
    pub already: usize,
    /// Files an earlier sync linked that this lock no longer names, removed.
    pub removed: usize,
}

/// Gets a blob into the cache through `get` (which fills `part`, resuming what is there) and
/// stores it only when it hashes to `sha` with `size` bytes. A download that does not is deleted,
/// reported, and fetched again from the start; after [`FETCH_ATTEMPTS`] it fails the sync. Nothing
/// that failed the check is ever in the cache, so nothing can link it.
pub fn fetch_verified(
    store: &Store,
    tmp: &Path,
    sha: &str,
    size: u64,
    path: &str,
    get: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    let part = tmp.join(format!("{sha}.part"));
    for attempt in 1..=FETCH_ATTEMPTS {
        let got = get(&part).and_then(|()| Ok(store::hash_file(&part)?));
        match got {
            Ok((got, len)) if got == sha && len == size => return store.store(&part, sha),
            Ok((got, len)) => {
                let _ = std::fs::remove_file(&part);
                (store.log)(&format!(
                    "{path}: the download hashed to {got} ({len} bytes), the lock says {sha} ({size} bytes); refetching ({attempt}/{FETCH_ATTEMPTS})"
                ));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                if attempt == FETCH_ATTEMPTS {
                    return Err(e).with_context(|| format!("{path}: fetching {sha}"));
                }
                (store.log)(&format!(
                    "{path}: {e:#}; refetching ({attempt}/{FETCH_ATTEMPTS})"
                ));
            }
        }
    }
    bail!(
        "{path}: {FETCH_ATTEMPTS} downloads of {sha} all failed the hash: the origin's copy is wrong (`assetsync pin` from a good tree re-uploads it) or the lock is"
    )
}

/// Syncs the lock at `lock_path` into `dest`.
pub fn sync(lock_path: &Path, dest: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let lock = Lock::read(lock_path)?;
    let name = lock_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lock");
    sync_lock(&lock, name, dest, opts).with_context(|| lock_path.display().to_string())
}

/// Syncs `lock` into `dest`. `name` tells this lock's stamp (`.assetsync-<name>`, the paths it
/// linked) from another lock's synced into the same tree.
pub fn sync_lock(lock: &Lock, name: &str, dest: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let store = opts.store;
    let log = &store.log;
    let started = Instant::now();
    let mut lock = lock.clone();
    lock.retain_under(&opts.only);
    if !opts.only.is_empty() && lock.entries.is_empty() {
        bail!("the lock names nothing under {}", opts.only.join(", "));
    }
    let blobs = lock.blobs();
    let mut report = SyncReport {
        files: lock.entries.len(),
        blobs: blobs.len(),
        ..SyncReport::default()
    };

    let mut missing: Vec<_> = blobs
        .iter()
        .filter(|(sha, size, _)| !store.has_whole(sha, *size))
        .collect();
    if !missing.is_empty() {
        let Some(origin) = opts.origin else {
            bail!(
                "the cache ({}) lacks {} of the lock's blobs and no origin was given",
                store.root.display(),
                missing.len()
            );
        };
        missing.sort_by_key(|b| std::cmp::Reverse(b.1));
        report.fetched = missing.len();
        report.fetched_bytes = missing.iter().map(|b| b.1).sum();
        log(&format!(
            "fetching {} blobs ({})",
            missing.len(),
            mb(report.fetched_bytes)
        ));
        let progress = Progress::new(log, "fetched", missing.len());
        let tmp = store.tmp_dir()?;
        let workers = if opts.workers == 0 {
            FETCH_WORKERS
        } else {
            opts.workers
        };
        par(&missing, workers, |(sha, size, path)| {
            fetch_verified(store, &tmp, sha, *size, path, |part| {
                origin.get(sha, *size, part)
            })?;
            progress.tick();
            Ok(())
        })?;
    }
    store.record_use(blobs.iter().map(|(sha, _, path)| (*sha, *path)))?;

    let fetched = started.elapsed();
    let stamp = format!(".assetsync-{name}");
    let (linked, already, removed) = if !opts.only.is_empty() {
        // A subset: nothing is stamped or removed, only linked.
        let results = par(&lock.entries, local_workers(), |e| {
            store.link_into(&e.sha, &dest.join(&e.path))
        })?;
        let linked = results.iter().filter(|b| **b).count();
        (linked, results.len() - linked, 0)
    } else if dest.exists() {
        materialize(store, &lock, dest, &stamp)?
    } else {
        // Built whole beside its place and renamed in: nothing ever reads half a tree.
        let staging = dest.with_file_name(format!(
            ".{}.staging-{}",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("assets"),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let r = materialize(store, &lock, &staging, &stamp)?;
        std::fs::rename(&staging, dest)?;
        r
    };
    report.linked = linked;
    report.already = already;
    report.removed = removed;
    log(&format!(
        "{} files: {linked} linked, {already} already in place, {removed} removed; fetch {:.1}s, link {:.1}s",
        lock.entries.len(),
        fetched.as_secs_f64(),
        (started.elapsed() - fetched).as_secs_f64()
    ));
    Ok(report)
}

/// Links every entry into `into`, replacing each by temp-then-rename, and removes what an earlier
/// sync linked that this lock no longer names. Safe to run again after a run that died half way:
/// the stamp of paths this tree may hold is widened to old plus new before anything moves, a file
/// that is missing or different is replaced, and link temporaries a dead run left are swept.
/// Returns (linked, already right, removed).
fn materialize(
    store: &Store,
    lock: &Lock,
    into: &Path,
    stamp: &str,
) -> Result<(usize, usize, usize)> {
    let stamp_path = into.join(stamp);
    let prev = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    let now: HashSet<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
    let new_list: String = lock
        .entries
        .iter()
        .map(|e| format!("{}\n", e.path))
        .collect();

    std::fs::create_dir_all(into)?;
    let both: String = prev
        .lines()
        .filter(|p| !now.contains(p))
        .map(|p| format!("{p}\n"))
        .collect();
    std::fs::write(&stamp_path, format!("{both}{new_list}"))?;

    let results = par(&lock.entries, local_workers(), |e| {
        store.link_into(&e.sha, &into.join(&e.path))
    })?;
    let linked = results.iter().filter(|b| **b).count();

    let mut removed = 0;
    // A stamp is this crate's own file, but a stamp edited by hand must not reach outside.
    for old in prev
        .lines()
        .filter(|p| !now.contains(p) && crate::lock::is_safe_path(p))
    {
        let file = into.join(old);
        if std::fs::remove_file(&file).is_ok() {
            removed += 1;
            let mut dir = file.parent();
            while let Some(d) = dir.filter(|d| *d != into) {
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    }
    let dirs: BTreeSet<&Path> = lock
        .entries
        .iter()
        .filter_map(|e| Path::new(&e.path).parent())
        .collect();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(into.join(d)) else {
            continue;
        };
        for f in rd.flatten() {
            if f.file_name().to_string_lossy().contains(LINK_TMP) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    std::fs::write(&stamp_path, new_list)?;
    Ok((linked, results.len() - linked, removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Entry;
    use crate::store::tests::store_in;
    use std::cell::Cell;
    use std::fs;

    #[test]
    fn a_corrupt_download_is_refetched_and_never_stored() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let tmp = store.tmp_dir().unwrap();
        let good = b"the real bytes".to_vec();
        let sha = store::hash_bytes(&good);
        // A flipped byte, then a truncation, then the real thing.
        let mut flipped = good.clone();
        flipped[3] ^= 1;
        let tries: Vec<Vec<u8>> = vec![flipped, good[..5].to_vec(), good.clone()];
        let n = Cell::new(0);
        fetch_verified(&store, &tmp, &sha, good.len() as u64, "assets/x", |part| {
            fs::write(part, &tries[n.get()])?;
            n.set(n.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(n.get(), 3);
        assert_eq!(fs::read(store.blob(&sha)).unwrap(), good);
        assert!(!tmp.join(format!("{sha}.part")).exists());
    }

    #[test]
    fn a_blob_that_never_hashes_fails_the_sync_and_stays_out_of_the_cache() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let tmp = store.tmp_dir().unwrap();
        let sha = store::hash_bytes(b"the real bytes");
        let n = Cell::new(0);
        let err = fetch_verified(&store, &tmp, &sha, 14, "assets/x", |part| {
            n.set(n.get() + 1);
            Ok(fs::write(part, b"not the bytes!")?)
        })
        .unwrap_err();
        assert_eq!(n.get(), FETCH_ATTEMPTS);
        assert!(format!("{err:#}").contains("assets/x"), "{err:#}");
        assert!(!store.has(&sha));
    }

    pub(crate) fn lock_of(files: &[(&str, &[u8])], store: &Store) -> Lock {
        let mut entries = Vec::new();
        for (path, bytes) in files {
            let sha = store::hash_bytes(bytes);
            store.put(&sha, bytes).unwrap();
            entries.push(Entry {
                sha,
                size: bytes.len() as u64,
                path: path.to_string(),
            });
        }
        let mut lock = Lock {
            header: vec![],
            entries,
        };
        lock.sort();
        lock
    }

    #[test]
    fn sync_removes_what_the_new_lock_dropped_and_repairs_a_half_built_tree() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let into = d.path().join("assets");
        let old = lock_of(
            &[
                ("a/one.bin", b"1"),
                ("a/b/two.bin", b"2"),
                ("keep.bin", b"k"),
            ],
            &store,
        );
        let (linked, _, _) = materialize(&store, &old, &into, ".stamp").unwrap();
        assert_eq!(linked, 3);

        // The new lock drops a file and changes one; the old run left damage behind.
        let new = lock_of(
            &[
                ("a/one.bin", b"1 changed"),
                ("keep.bin", b"k"),
                ("new.bin", b"n"),
            ],
            &store,
        );
        fs::remove_file(into.join("keep.bin")).unwrap();
        fs::write(into.join("keep.bin"), "something else\n").unwrap();
        fs::write(into.join(format!("a/.one.bin{LINK_TMP}9")), "half").unwrap();
        fs::write(into.join("authored.bsn"), "mine").unwrap();
        let (_, _, removed) = materialize(&store, &new, &into, ".stamp").unwrap();

        assert_eq!(removed, 1);
        assert!(!into.join("a/b/two.bin").exists(), "a dropped file stays");
        assert!(!into.join("a/b").exists(), "its empty directory stays");
        assert_eq!(fs::read(into.join("a/one.bin")).unwrap(), b"1 changed");
        assert_eq!(fs::read(into.join("keep.bin")).unwrap(), b"k");
        assert_eq!(fs::read(into.join("new.bin")).unwrap(), b"n");
        assert!(!into.join(format!("a/.one.bin{LINK_TMP}9")).exists());
        assert_eq!(fs::read(into.join("authored.bsn")).unwrap(), b"mine");
        // Run again: nothing to do.
        assert_eq!(materialize(&store, &new, &into, ".stamp").unwrap().0, 0);
    }

    #[test]
    fn a_sync_that_died_before_removing_still_removes_next_time() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let into = d.path().join("assets");
        let old = lock_of(&[("gone.bin", b"g"), ("a.bin", b"a")], &store);
        materialize(&store, &old, &into, ".stamp").unwrap();
        let new = lock_of(&[("a.bin", b"a")], &store);
        // Dies after the widened stamp is written and a link is done, before removal.
        let stamp = fs::read_to_string(into.join(".stamp")).unwrap();
        fs::write(into.join(".stamp"), format!("{stamp}gone.bin\n")).unwrap();
        materialize(&store, &new, &into, ".stamp").unwrap();
        assert!(!into.join("gone.bin").exists());
    }

    #[test]
    fn a_new_destination_is_built_in_staging_and_a_partial_sync_only_links() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let lock = lock_of(&[("voice/a.mp3", b"a"), ("words/b.mp3", b"b")], &store);
        let dest = d.path().join("clips");
        let opts = SyncOptions {
            store: &store,
            origin: None,
            only: vec![],
            workers: 0,
        };
        let r = sync_lock(&lock, "voice.lock", &dest, &opts).unwrap();
        assert_eq!((r.files, r.linked, r.fetched), (2, 2, 0));
        assert!(dest.join(".assetsync-voice.lock").is_file());
        let leftovers: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("staging"))
            .collect();
        assert!(leftovers.is_empty());

        let other = d.path().join("other");
        let opts = SyncOptions {
            only: vec!["voice/".into()],
            ..opts
        };
        let r = sync_lock(&lock, "voice.lock", &other, &opts).unwrap();
        assert_eq!((r.files, r.linked), (1, 1));
        assert!(other.join("voice/a.mp3").is_file());
        assert!(!other.join("words").exists());
        assert!(!other.join(".assetsync-voice.lock").exists());
        let opts = SyncOptions {
            only: vec!["nope/".into()],
            ..opts
        };
        assert!(sync_lock(&lock, "voice.lock", &other, &opts).is_err());
    }

    #[test]
    fn a_missing_blob_with_no_origin_says_so() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = store::hash_bytes(b"nowhere");
        let lock = Lock::parse(&format!("{sha} 7 x.bin\n")).unwrap();
        let opts = SyncOptions {
            store: &store,
            origin: None,
            only: vec![],
            workers: 0,
        };
        let e = sync_lock(&lock, "l", &d.path().join("t"), &opts).unwrap_err();
        assert!(e.to_string().contains("no origin"), "{e}");
    }
}
