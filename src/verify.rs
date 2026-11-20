//! Verify: does a tree hold exactly the bytes its lock names? Every file is hashed; nothing is
//! changed.

use std::path::Path;

use anyhow::Result;

use crate::lock::Lock;
use crate::par::{local_workers, par};
use crate::store;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Entries checked, and how many matched.
    pub files: usize,
    pub ok: usize,
    /// Paths the lock names that are not in the tree (or are not regular files).
    pub missing: Vec<String>,
    /// Paths whose size or sha256 differs from the lock's.
    pub bad: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty() && self.bad.is_empty()
    }
}

enum Seen {
    Ok,
    Missing,
    Bad,
}

/// Verifies the tree `dir` against the lock at `lock_path`.
pub fn verify(lock_path: &Path, dir: &Path) -> Result<VerifyReport> {
    verify_lock(&Lock::read(lock_path)?, dir)
}

pub fn verify_lock(lock: &Lock, dir: &Path) -> Result<VerifyReport> {
    let seen = par(&lock.entries, local_workers(), |e| {
        let file = dir.join(&e.path);
        let Ok(meta) = std::fs::metadata(&file) else {
            return Ok(Seen::Missing);
        };
        if !meta.is_file() {
            return Ok(Seen::Missing);
        }
        if meta.len() != e.size {
            return Ok(Seen::Bad);
        }
        Ok(match store::hash_file(&file) {
            Ok((sha, _)) if sha == e.sha => Seen::Ok,
            Ok(_) => Seen::Bad,
            Err(_) => Seen::Missing,
        })
    })?;
    let mut r = VerifyReport {
        files: lock.entries.len(),
        ..VerifyReport::default()
    };
    for (e, s) in lock.entries.iter().zip(seen) {
        match s {
            Seen::Ok => r.ok += 1,
            Seen::Missing => r.missing.push(e.path.clone()),
            Seen::Bad => r.bad.push(e.path.clone()),
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn verify_finds_missing_and_changed_files() {
        let d = tempfile::tempdir().unwrap();
        let a = store::hash_bytes(b"a");
        let b = store::hash_bytes(b"bb");
        let c = store::hash_bytes(b"c");
        let lock = Lock::parse(&format!("{a} 1 a.txt\n{b} 2 b.txt\n{c} 1 dir/c.txt\n")).unwrap();
        fs::write(d.path().join("a.txt"), b"a").unwrap();
        fs::write(d.path().join("b.txt"), b"BB").unwrap();
        let r = verify_lock(&lock, d.path()).unwrap();
        assert_eq!(r.files, 3);
        assert_eq!(r.ok, 1);
        assert_eq!(r.bad, ["b.txt"]);
        assert_eq!(r.missing, ["dir/c.txt"]);
        assert!(!r.is_ok());
    }
}
