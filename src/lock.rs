//! The lock: one sorted text file per asset set, committed to git. A line is
//! `<sha256> <size> <path>` (the path is the rest of the line, relative to the tree the lock
//! describes); header lines start with `#` and carry `# key value` provenance the caller chooses.
//!
//! The format is zero's `assets.lock`, byte for byte: a lock written by either reads in the other.

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Lowercase hex sha256 of the file's bytes.
    pub sha: String,
    pub size: u64,
    /// Relative to the tree, `/`-separated; may hold spaces.
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    /// `# key value` header lines, in file order.
    pub header: Vec<(String, String)>,
    /// Sorted by path, each path once.
    pub entries: Vec<Entry>,
}

/// True for a lowercase hex sha256.
pub fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A path a lock may name: relative, inside its tree (no `..`, no root), not empty. A lock is
/// data from a commit, and sync writes where it says.
pub fn is_safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.contains('\0')
        && Path::new(p)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

impl Lock {
    pub fn read(path: &Path) -> Result<Lock> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "{}: no lock; `assetsync pin` writes it from a tree",
                path.display()
            )
        })?;
        Lock::parse(&text).with_context(|| path.display().to_string())
    }

    pub fn parse(text: &str) -> Result<Lock> {
        let mut lock = Lock::default();
        for (n, line) in text.lines().enumerate() {
            let n = n + 1;
            if let Some(h) = line.strip_prefix('#') {
                let h = h.trim();
                let (k, v) = h.split_once(' ').unwrap_or((h, ""));
                lock.header.push((k.to_string(), v.trim().to_string()));
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(3, ' ');
            let (Some(sha), Some(size), Some(path)) = (it.next(), it.next(), it.next()) else {
                bail!("line {n}: expected `<sha256> <size> <path>`");
            };
            if !is_sha(sha) {
                bail!("line {n}: `{sha}` is not a lowercase sha256");
            }
            let size = size
                .parse()
                .with_context(|| format!("line {n}: size `{size}`"))?;
            if !is_safe_path(path) {
                bail!("line {n}: `{path}` is not a relative path inside the tree");
            }
            lock.entries.push(Entry {
                sha: sha.to_string(),
                size,
                path: path.to_string(),
            });
        }
        if !lock.entries.windows(2).all(|w| w[0].path < w[1].path) {
            bail!("entries are not sorted by path, or a path repeats: re-run `assetsync pin`");
        }
        Ok(lock)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.header {
            out.push_str(&format!("# {k} {v}\n"));
        }
        for e in &self.entries {
            out.push_str(&format!("{} {} {}\n", e.sha, e.size, e.path));
        }
        out
    }

    /// Written to a temporary beside it and renamed over it: a reader sees the old lock or the
    /// new one.
    pub fn write(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, self.render()).with_context(|| tmp.display().to_string())?;
        std::fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
        Ok(())
    }

    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.path.cmp(&b.path));
    }

    /// The first header value under `key`.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Distinct blobs with their size, the first path each is known by.
    pub fn blobs(&self) -> Vec<(&str, u64, &str)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for e in &self.entries {
            if seen.insert(e.sha.as_str()) {
                out.push((e.sha.as_str(), e.size, e.path.as_str()));
            }
        }
        out
    }

    /// Keeps the entries under any of `prefixes` (all of them when there are none).
    pub fn retain_under(&mut self, prefixes: &[String]) {
        if !prefixes.is_empty() {
            self.entries
                .retain(|e| prefixes.iter().any(|p| e.path.starts_with(p.as_str())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_round_trips_and_stays_sorted() {
        let sha = "a".repeat(64);
        let text = format!("# importers abc\n{sha} 5 assets/a b.png\n{sha} 7 assets/c.png\n");
        let lock = Lock::parse(&text).unwrap();
        assert_eq!(lock.entries[0].path, "assets/a b.png");
        assert_eq!(lock.header("importers"), Some("abc"));
        assert_eq!(lock.render(), text);
        assert!(Lock::parse(&format!("{sha} 1 z\n{sha} 1 a\n")).is_err());
        assert!(Lock::parse("zz 1 a\n").is_err());
    }

    #[test]
    fn a_path_outside_the_tree_is_refused() {
        let sha = "b".repeat(64);
        for bad in ["../x", "a/../../x", "/etc/passwd"] {
            assert!(Lock::parse(&format!("{sha} 1 {bad}\n")).is_err(), "{bad}");
        }
        assert!(Lock::parse(&format!("{sha} 1 a/./b..c\n")).is_ok());
    }

    #[test]
    fn blobs_are_distinct_and_prefixes_filter() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let mut lock =
            Lock::parse(&format!("{a} 1 raw/x\n{a} 1 raw/y\n{b} 2 voice/z.mp3\n")).unwrap();
        assert_eq!(lock.blobs().len(), 2);
        lock.retain_under(&["voice/".to_string()]);
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "voice/z.mp3");
    }
}
