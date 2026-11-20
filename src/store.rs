//! The local content-addressed store (CAS): each file stored once, named by the sha256 of its
//! bytes, at `<root>/<ab>/<sha256>`, mode 0444, never written in place. A synced tree is
//! hardlinks into it, so a write in place fails with EACCES and every save has to be
//! delete-then-create or temp-then-rename. A tree on another filesystem gets copies instead.
//!
//! `index.tsv` beside the blobs is the last-used record `gc` reads: `sha <TAB> unix-seconds
//! <TAB> path`, updated by every pin and sync for each blob a lock names. (atime is no use: many
//! disks are mounted noatime.)

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::Log;

pub struct Store {
    pub root: PathBuf,
    pub log: Log,
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// sha256 of a file, streamed: (hex, bytes).
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex(&h.finalize()), total))
}

/// sha256 of bytes in memory.
pub fn hash_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The default store root: `$ASSETSYNC_CACHE`, else `$XDG_CACHE_HOME/assetsync`, else
/// `~/.cache/assetsync`.
pub fn default_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("ASSETSYNC_CACHE").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p).join("assetsync"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set, and no cache dir was given")?;
    Ok(PathBuf::from(home).join(".cache/assetsync"))
}

/// A name no other worker or process is using.
fn unique() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The marker in a link temporary's name, which a sync sweeps when a dead run left one.
pub(crate) const LINK_TMP: &str = ".assetsync-link-";

/// ext4 allows 65,000 links to one inode: a blob linked by more trees than that gets a second
/// copy (`<sha>.1`, ...).
fn is_emlink(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EMLINK)
}

/// The blob and the tree are on different filesystems: copy instead of link.
fn is_cross_device(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::EXDEV)
}

impl Store {
    /// The store at `root`, created if missing, logging to stderr.
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        Store::open_with(root, crate::stderr_log())
    }

    pub fn open_with(root: impl Into<PathBuf>, log: Log) -> Result<Store> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| root.display().to_string())?;
        Ok(Store { root, log })
    }

    pub fn blob(&self, sha: &str) -> PathBuf {
        self.root.join(&sha[..2]).join(sha)
    }

    /// Copy `n` of a blob: 0 is the blob itself.
    fn copy_path(&self, sha: &str, n: u32) -> PathBuf {
        if n == 0 {
            self.blob(sha)
        } else {
            self.root.join(&sha[..2]).join(format!("{sha}.{n}"))
        }
    }

    pub fn has(&self, sha: &str) -> bool {
        self.blob(sha).is_file()
    }

    /// The blob is there at its full size. A shorter file (a cache hurt by a crash or a full disk)
    /// is removed so the next fetch replaces it.
    pub fn has_whole(&self, sha: &str, size: u64) -> bool {
        let blob = self.blob(sha);
        match fs::metadata(&blob) {
            Ok(m) if m.len() == size => true,
            Ok(_) => {
                (self.log)(&format!(
                    "cache blob {sha} is not {size} bytes; dropping it to refetch"
                ));
                let _ = fs::remove_file(&blob);
                false
            }
            Err(_) => false,
        }
    }

    pub fn tmp_dir(&self) -> Result<PathBuf> {
        let d = self.root.join("tmp");
        fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// Adds a file's content to the cache as `sha` (already hashed by the caller): a hardlink of
    /// the file itself (then 0444, so the tree's own file is the blob), else a copy across
    /// filesystems.
    pub fn ingest(&self, file: &Path, sha: &str) -> Result<()> {
        let dest = self.blob(sha);
        if dest.is_file() {
            return Ok(());
        }
        fs::create_dir_all(dest.parent().expect("blob has a dir"))?;
        let tmp = self.tmp_dir()?.join(format!("{sha}.{}.in", unique()));
        let _ = fs::remove_file(&tmp);
        if fs::hard_link(file, &tmp).is_err() {
            fs::copy(file, &tmp).with_context(|| format!("copy {}", file.display()))?;
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        fs::rename(&tmp, &dest)?;
        Ok(())
    }

    /// Puts `bytes`, whose sha256 is `sha`, into the store (kept if already there).
    pub fn put(&self, sha: &str, bytes: &[u8]) -> Result<PathBuf> {
        let dest = self.blob(sha);
        if dest.is_file() {
            return Ok(dest);
        }
        let tmp = self.tmp_dir()?.join(format!("{sha}.{}.in", unique()));
        fs::write(&tmp, bytes)?;
        self.store(&tmp, sha)?;
        Ok(dest)
    }

    /// Moves a verified download into place, read-only.
    pub fn store(&self, part: &Path, sha: &str) -> Result<()> {
        let dest = self.blob(sha);
        fs::create_dir_all(dest.parent().expect("blob has a dir"))?;
        fs::set_permissions(part, fs::Permissions::from_mode(0o444))?;
        fs::rename(part, &dest)?;
        Ok(())
    }

    /// Makes `dest` a hardlink of the blob, replacing whatever is there by temp-then-rename (never
    /// a write in place). Returns false when `dest` already is this blob (a link of it, or on
    /// another filesystem a copy with its bytes). A blob at its link limit gets another copy.
    pub fn link_into(&self, sha: &str, dest: &Path) -> Result<bool> {
        let existing = fs::symlink_metadata(dest).ok();
        if let Some(dm) = &existing {
            for n in 0.. {
                let c = self.copy_path(sha, n);
                match fs::metadata(&c) {
                    Ok(bm) if bm.ino() == dm.ino() && bm.dev() == dm.dev() => return Ok(false),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = dest.with_file_name(format!(
            ".{}{LINK_TMP}{}",
            dest.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
            unique()
        ));
        let _ = fs::remove_file(&tmp);
        let mut n = 0;
        loop {
            let c = self.copy_path(sha, n);
            if !c.is_file() {
                // Every copy so far is full: start the next one from copy 0.
                if n == 0 {
                    bail!(
                        "blob {sha} is missing from {}: run `assetsync sync`",
                        self.root.display()
                    );
                }
                let from = self.copy_path(sha, 0);
                fs::copy(&from, &c)?;
                fs::set_permissions(&c, fs::Permissions::from_mode(0o444))?;
            }
            match fs::hard_link(&c, &tmp) {
                Ok(()) => break,
                Err(e) if is_emlink(&e) => n += 1,
                Err(e) if is_cross_device(&e) => return self.copy_into(sha, dest, &tmp, existing),
                Err(e) => return Err(e).with_context(|| format!("link {}", dest.display())),
            }
        }
        fs::rename(&tmp, dest).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(true)
    }

    /// `link_into` across filesystems: a copy, unless what is there already has the blob's bytes.
    fn copy_into(
        &self,
        sha: &str,
        dest: &Path,
        tmp: &Path,
        existing: Option<fs::Metadata>,
    ) -> Result<bool> {
        let blob = self.blob(sha);
        if let Some(dm) = existing
            && dm.is_file()
            && fs::metadata(&blob).is_ok_and(|b| b.len() == dm.len())
            && hash_file(dest).is_ok_and(|(got, _)| got == sha)
        {
            return Ok(false);
        }
        fs::copy(&blob, tmp).with_context(|| format!("copy into {}", dest.display()))?;
        fs::rename(tmp, dest).inspect_err(|_| {
            let _ = fs::remove_file(tmp);
        })?;
        Ok(true)
    }

    /// Every blob file in the cache: (sha, path, size). Extra copies (`<sha>.<n>`) are reported
    /// under their sha too.
    pub fn blobs(&self) -> Result<Vec<(String, PathBuf, u64)>> {
        let mut out = Vec::new();
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            let name = shard.file_name().to_string_lossy().to_string();
            if name.len() != 2 || !shard.file_type()?.is_dir() {
                continue;
            }
            for f in fs::read_dir(shard.path())? {
                let f = f?;
                let fname = f.file_name().to_string_lossy().to_string();
                let sha = fname.split('.').next().unwrap_or("").to_string();
                if crate::lock::is_sha(&sha) {
                    out.push((sha, f.path(), f.metadata()?.len()));
                }
            }
        }
        Ok(out)
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.tsv")
    }

    pub fn read_index(&self) -> HashMap<String, (u64, String)> {
        let mut m = HashMap::new();
        if let Ok(text) = fs::read_to_string(self.index_path()) {
            for l in text.lines() {
                let mut it = l.splitn(3, '\t');
                if let (Some(s), Some(t), Some(p)) = (it.next(), it.next(), it.next())
                    && let Ok(t) = t.parse()
                {
                    m.insert(s.to_string(), (t, p.to_string()));
                }
            }
        }
        m
    }

    /// Records that these blobs are in use now, under these paths (`gc` reads it).
    pub fn record_use<'a>(&self, used: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<()> {
        let t = now();
        self.edit_index(|idx| {
            for (sha, path) in used {
                idx.insert(sha.to_string(), (t, path.to_string()));
            }
        })
    }

    /// Drops the index rows of blobs that are gone.
    pub fn forget(&self, gone: &HashSet<String>) -> Result<()> {
        self.edit_index(|idx| idx.retain(|sha, _| !gone.contains(sha)))
    }

    /// Rewrites the index under an exclusive lock, so two processes never lose each other's rows.
    fn edit_index(&self, f: impl FnOnce(&mut HashMap<String, (u64, String)>)) -> Result<()> {
        let lock = File::create(self.root.join("index.lock"))?;
        // SAFETY: flock on a descriptor we own; released when `lock` drops.
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX);
        }
        let mut idx = self.read_index();
        f(&mut idx);
        let mut rows: Vec<_> = idx.into_iter().collect();
        rows.sort();
        let tmp = self.root.join(format!("index.tsv.{}.tmp", unique()));
        {
            let mut w = io::BufWriter::new(File::create(&tmp)?);
            for (sha, (t, p)) in rows {
                writeln!(w, "{sha}\t{t}\t{p}")?;
            }
            w.flush()?;
        }
        fs::rename(&tmp, self.index_path())?;
        drop(lock);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn store_in(dir: &Path) -> Store {
        Store::open_with(dir.join("store"), crate::quiet_log()).unwrap()
    }

    fn put(store: &Store, bytes: &[u8]) -> String {
        let d = store.root.join("src");
        fs::create_dir_all(&d).unwrap();
        let f = d.join(format!("f{}", bytes.len()));
        fs::write(&f, bytes).unwrap();
        let (sha, _) = hash_file(&f).unwrap();
        store.ingest(&f, &sha).unwrap();
        sha
    }

    /// A linked file is the blob, so writing it in place is refused (0444), and replacing it
    /// (delete-then-create, temp-then-rename) leaves the blob.
    #[test]
    fn rewriting_a_linked_file_never_touches_the_blob() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = put(&store, b"baked");
        let tree = d.path().join("tree/assets/a.bin");
        assert!(store.link_into(&sha, &tree).unwrap());
        assert!(!store.link_into(&sha, &tree).unwrap(), "already linked");

        // In place: refused (unless the tests run as root, which ignores the mode).
        if unsafe { libc::geteuid() } != 0 {
            let err = fs::OpenOptions::new().write(true).open(&tree).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        // Delete then create: the tree has new content, the blob the old.
        fs::remove_file(&tree).unwrap();
        fs::write(&tree, b"rebaked").unwrap();
        assert_eq!(fs::read(store.blob(&sha)).unwrap(), b"baked");
        assert_ne!(
            fs::metadata(&tree).unwrap().ino(),
            fs::metadata(store.blob(&sha)).unwrap().ino()
        );

        // Temp then rename over a link.
        assert!(
            store.link_into(&sha, &tree).unwrap(),
            "relinked over the new file"
        );
        let tmp = tree.with_file_name("a.bin.tmp");
        fs::write(&tmp, b"again").unwrap();
        fs::rename(&tmp, &tree).unwrap();
        assert_eq!(fs::read(store.blob(&sha)).unwrap(), b"baked");
    }

    /// ext4 allows 65,000 links to an inode: the next tree gets a second copy of the blob.
    /// Makes 65,001 links, so it runs only when asked (`cargo test -- --ignored`).
    #[test]
    #[ignore = "makes 65,001 hardlinks"]
    fn a_blob_at_its_link_limit_gets_a_second_copy() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = put(&store, b"popular");
        let dir = d.path().join("trees");
        fs::create_dir_all(&dir).unwrap();
        let limit = 65_001usize;
        let mut linked = 0;
        for i in 0..limit {
            if store.link_into(&sha, &dir.join(format!("t{i}"))).unwrap() {
                linked += 1;
            }
        }
        assert_eq!(linked, limit);
        // Past the limit (on ext4; elsewhere the limit is higher and one copy serves all).
        let second = store.root.join(&sha[..2]).join(format!("{sha}.1"));
        if fs::metadata(store.blob(&sha)).unwrap().nlink() >= 65_000 {
            assert!(second.is_file());
            assert_eq!(fs::read(&second).unwrap(), b"popular");
        }
    }

    #[test]
    fn last_use_is_recorded_per_blob_with_its_path_and_forgotten() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = put(&store, b"x");
        store.record_use([(sha.as_str(), "assets/x.png")]).unwrap();
        let idx = store.read_index();
        assert_eq!(idx[&sha].1, "assets/x.png");
        assert!(idx[&sha].0 >= now() - 5);
        store.forget(&[sha.clone()].into()).unwrap();
        assert!(!store.read_index().contains_key(&sha));
    }

    #[test]
    fn a_put_blob_is_read_only_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = hash_bytes(b"baked");
        let blob = store.put(&sha, b"baked").unwrap();
        assert_eq!(blob, store.root.join(&sha[..2]).join(&sha));
        assert_eq!(fs::metadata(&blob).unwrap().mode() & 0o777, 0o444);
        store.put(&sha, b"other").unwrap();
        assert_eq!(fs::read(&blob).unwrap(), b"baked");
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn a_short_cache_blob_is_dropped_to_be_refetched() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = hash_bytes(b"0123456789");
        fs::create_dir_all(store.blob(&sha).parent().unwrap()).unwrap();
        fs::write(store.blob(&sha), b"0123").unwrap();
        assert!(!store.has_whole(&sha, 10));
        assert!(!store.has(&sha));
    }

    #[test]
    fn a_copy_with_the_blobs_bytes_counts_as_in_place() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = put(&store, b"bytes");
        let dest = d.path().join("other/a.bin");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"bytes").unwrap();
        let tmp = dest.with_file_name("t");
        let existing = fs::symlink_metadata(&dest).ok();
        assert!(!store.copy_into(&sha, &dest, &tmp, existing).unwrap());
        fs::remove_file(&dest).unwrap();
        fs::write(&dest, b"BYTES").unwrap();
        let existing = fs::symlink_metadata(&dest).ok();
        assert!(store.copy_into(&sha, &dest, &tmp, existing).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), b"bytes");
    }
}
