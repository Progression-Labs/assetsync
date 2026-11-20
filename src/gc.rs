//! Local gc: a review of the blobs in the cache that can go, and a deletion. A blob is a
//! candidate when none of the given locks names it AND the cache's last-used record (store.rs
//! `index.tsv`) is older than `unused_for`. The CLI deletes only when the person at the terminal
//! types `delete`; with no terminal it prints the review and refuses, and no flag skips that.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::lock::Lock;
use crate::store::Store;

/// `90d`, `12w` or `3m` (30 days) as seconds.
pub fn parse_age(s: &str) -> Result<u64, String> {
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = n
        .parse()
        .map_err(|_| format!("`{s}`: expected 90d, 12w or 3m"))?;
    let day = 86_400;
    match unit {
        "d" => Ok(n * day),
        "w" => Ok(n * 7 * day),
        "m" => Ok(n * 30 * day),
        _ => Err(format!("`{s}`: expected 90d, 12w or 3m")),
    }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub sha: String,
    pub size: u64,
    pub last_used: u64,
    /// The path it was last used under.
    pub path: String,
    /// Its files in the cache (the blob and any extra copies).
    pub files: Vec<PathBuf>,
}

/// Every sha the locks at `paths` name. A path that is not there is skipped (a worktree's lock
/// that was never written), one that does not parse is an error: what it names could be swept.
pub fn live_blobs(paths: &[PathBuf]) -> Result<HashSet<String>> {
    let mut live = HashSet::new();
    for p in paths {
        if p.is_file() {
            live.extend(Lock::read(p)?.entries.into_iter().map(|e| e.sha));
        }
    }
    Ok(live)
}

/// The blobs in `store` outside `live` and last used before `now - unused_for`. A blob with no
/// record is stamped as used now (first sight), so a fresh cache or an older one is never swept
/// on its first review.
pub fn candidates(
    store: &Store,
    live: &HashSet<String>,
    unused_for: u64,
    now: u64,
) -> Result<Vec<Candidate>> {
    let index = store.read_index();
    let mut files: HashMap<String, (u64, Vec<PathBuf>)> = HashMap::new();
    for (sha, path, size) in store.blobs()? {
        let e = files.entry(sha).or_insert((size, Vec::new()));
        e.1.push(path);
    }
    let unseen: Vec<String> = files
        .keys()
        .filter(|s| !index.contains_key(*s) && !live.contains(*s))
        .cloned()
        .collect();
    if !unseen.is_empty() {
        store.record_use(unseen.iter().map(|s| (s.as_str(), "(unknown)")))?;
    }
    let mut out = Vec::new();
    for (sha, (size, files)) in files {
        if live.contains(&sha) {
            continue;
        }
        let (last_used, path) = index
            .get(&sha)
            .cloned()
            .unwrap_or((now, "(unknown)".to_string()));
        if now.saturating_sub(last_used) >= unused_for {
            out.push(Candidate {
                sha,
                size,
                last_used,
                path,
                files,
            });
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size).then(a.sha.cmp(&b.sha)));
    Ok(out)
}

/// Days since the epoch to a civil date (Howard Hinnant's algorithm).
fn date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Prints the candidates; returns their bytes.
pub fn review(c: &[Candidate], out: &mut impl Write) -> Result<u64> {
    let total: u64 = c.iter().map(|c| c.size).sum();
    for c in c {
        writeln!(
            out,
            "{:>10.1} MB  last used {}  {}  {}",
            c.size as f64 / 1e6,
            date(c.last_used),
            &c.sha[..12],
            c.path
        )?;
    }
    writeln!(
        out,
        "{} blobs, {:.2} GB would be freed (once no tree still links them)",
        c.len(),
        total as f64 / 1e9
    )?;
    Ok(total)
}

/// Deletes the candidates' files and their index rows. Returns how many blobs went.
pub fn delete(store: &Store, c: &[Candidate]) -> Result<usize> {
    for c in c {
        for f in &c.files {
            match std::fs::remove_file(f) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    store.forget(&c.iter().map(|c| c.sha.clone()).collect())?;
    Ok(c.len())
}

/// Deletes the candidates when `tty` and the person types `delete`. Returns how many blobs went.
pub fn confirm_and_delete(
    store: &Store,
    c: &[Candidate],
    tty: bool,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<usize> {
    if c.is_empty() {
        return Ok(0);
    }
    if !tty {
        bail!("not deleting: gc deletes only after you confirm at a terminal, and this has none");
    }
    write!(out, "Type `delete` to delete these {} blobs: ", c.len())?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if answer.trim() != "delete" {
        writeln!(out, "nothing deleted")?;
        return Ok(0);
    }
    delete(store, c)
}

/// Removes download leftovers (`tmp/*.part` and ingest temporaries) older than a day.
pub fn sweep_tmp(store: &Store, now: u64) -> Result<usize> {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(store.root.join("tmp")) else {
        return Ok(0);
    };
    for f in rd.flatten() {
        let old = f
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| now.saturating_sub(t.as_secs()) >= 86_400);
        if old && std::fs::remove_file(f.path()).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// `index.tsv`'s path, for a caller that reports it.
pub fn index_path(store: &Store) -> PathBuf {
    store.root.join("index.tsv")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, tests::store_in};
    use std::fs;

    fn blob(store: &Store, tag: &str) -> String {
        let sha = store::hash_bytes(tag.as_bytes());
        store.put(&sha, tag.as_bytes()).unwrap();
        sha
    }

    #[test]
    fn a_candidate_is_unlocked_and_unused_for_months() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let old = blob(&store, "old");
        let recent = blob(&store, "recent");
        let locked = blob(&store, "locked");
        let now = store::now();
        let rows = format!(
            "{old}\t{}\tassets/old.png\n{recent}\t{}\tassets/recent.png\n{locked}\t{}\tassets/locked.png\n",
            now - 100 * 86_400,
            now - 10 * 86_400,
            now - 100 * 86_400
        );
        fs::write(index_path(&store), rows).unwrap();

        let live: HashSet<String> = [locked.clone()].into();
        let c = candidates(&store, &live, 90 * 86_400, now).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].sha, old);
        assert_eq!(c[0].path, "assets/old.png");
        assert_eq!(delete(&store, &c).unwrap(), 1);
        assert!(!store.has(&old));
        assert!(!store.read_index().contains_key(&old));
    }

    #[test]
    fn a_blob_never_seen_is_stamped_not_swept() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = blob(&store, "fresh");
        let c = candidates(&store, &HashSet::new(), 90 * 86_400, store::now()).unwrap();
        assert!(c.is_empty());
        assert!(store.read_index().contains_key(&sha));
    }

    #[test]
    fn nothing_is_deleted_without_a_terminal_or_the_word() {
        let d = tempfile::tempdir().unwrap();
        let store = store_in(d.path());
        let sha = blob(&store, "x");
        let c = vec![Candidate {
            sha: sha.clone(),
            size: 1,
            last_used: 0,
            path: "p".into(),
            files: vec![store.blob(&sha)],
        }];
        let mut out = Vec::new();
        let err = confirm_and_delete(&store, &c, false, &mut "delete\n".as_bytes(), &mut out)
            .unwrap_err();
        assert!(err.to_string().contains("has none"));
        assert!(store.has(&sha));
        assert_eq!(
            confirm_and_delete(&store, &c, true, &mut "yes\n".as_bytes(), &mut out).unwrap(),
            0
        );
        assert!(store.has(&sha));
        assert_eq!(
            confirm_and_delete(&store, &c, true, &mut "delete\n".as_bytes(), &mut out).unwrap(),
            1
        );
        assert!(!store.has(&sha));
    }

    #[test]
    fn live_blobs_reads_every_lock_given() {
        let d = tempfile::tempdir().unwrap();
        let a = "a".repeat(64);
        let p = d.path().join("one.lock");
        fs::write(&p, format!("{a} 1 x\n")).unwrap();
        let live = live_blobs(&[p, d.path().join("absent.lock")]).unwrap();
        assert!(live.contains(&a));
        let bad = d.path().join("bad.lock");
        fs::write(&bad, "nonsense\n").unwrap();
        assert!(live_blobs(&[bad]).is_err());
    }

    #[test]
    fn ages_parse() {
        assert_eq!(parse_age("90d").unwrap(), 90 * 86_400);
        assert_eq!(parse_age("3m").unwrap(), 90 * 86_400);
        assert!(parse_age("soon").is_err());
        assert_eq!(date(0), "1970-01-01");
    }
}
