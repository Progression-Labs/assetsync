//! Pin and sync through both origins against an in-process fake server.

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use assetsync::{
    Http, HttpConfig, Lock, Origin, PinOptions, R2, R2Config, Retry, Store, SyncOptions,
};
use support::Server;

fn fast() -> Retry {
    Retry {
        attempts: 4,
        auth_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
}

fn http(server: &Server, token: Option<&str>, write: bool) -> Http {
    Http::new(
        HttpConfig {
            base_url: format!("{}/blobs/", server.base),
            token: token.map(str::to_string),
            write,
        },
        fast(),
        assetsync::quiet_log(),
    )
}

fn store(dir: &Path, name: &str) -> Store {
    Store::open_with(dir.join(name), assetsync::quiet_log()).unwrap()
}

/// A lock over `files` whose blobs are on `server`.
fn lock_on(server: &Server, dir: &Path, files: &[(&str, &[u8])]) -> std::path::PathBuf {
    let mut text = String::from("# source test\n");
    let mut rows: Vec<_> = files
        .iter()
        .map(|(p, b)| format!("{} {} {p}\n", server.add(b), b.len()))
        .collect();
    rows.sort_by(|a, b| a.split(' ').nth(2).cmp(&b.split(' ').nth(2)));
    text.extend(rows);
    let path = dir.join("voice.lock");
    fs::write(&path, text).unwrap();
    path
}

fn sync_with(
    store: &Store,
    origin: &dyn Origin,
    lock: &Path,
    dest: &Path,
) -> anyhow::Result<assetsync::SyncReport> {
    assetsync::sync(
        lock,
        dest,
        &SyncOptions {
            store,
            origin: Some(origin),
            only: vec![],
            workers: 8,
        },
    )
}

#[test]
fn an_http_sync_fetches_verifies_and_links_and_a_second_fetches_nothing() {
    let server = Server::start();
    let d = tempfile::tempdir().unwrap();
    let lock = lock_on(
        &server,
        d.path(),
        &[
            ("words/cat.mp3", b"cat clip"),
            ("words/dog.mp3", b"dog clip"),
            ("words/kitty.mp3", b"cat clip"),
        ],
    );
    let store = store(d.path(), "cache");
    let origin = http(&server, None, false);
    let dest = d.path().join("clips");
    let r = sync_with(&store, &origin, &lock, &dest).unwrap();
    assert_eq!((r.files, r.blobs, r.fetched, r.linked), (3, 2, 2, 3));
    assert_eq!(r.fetched_bytes, 16);
    assert_eq!(fs::read(dest.join("words/kitty.mp3")).unwrap(), b"cat clip");
    assert!(assetsync::verify(&lock, &dest).unwrap().is_ok());

    let r = sync_with(&store, &origin, &lock, &dest).unwrap();
    assert_eq!((r.fetched, r.linked, r.already), (0, 0, 3));
}

#[test]
fn a_corrupt_blob_is_refetched_and_one_that_stays_corrupt_never_lands() {
    let server = Server::start();
    let d = tempfile::tempdir().unwrap();
    let lock = lock_on(&server, d.path(), &[("a.mp3", b"the real clip")]);
    let sha = Lock::read(&lock).unwrap().entries[0].sha.clone();
    let origin = http(&server, None, false);

    // Corrupt once: the second download is kept.
    server.state.lock().unwrap().corrupt.insert(sha.clone(), 1);
    let cache = store(d.path(), "cache1");
    sync_with(&cache, &origin, &lock, &d.path().join("one")).unwrap();
    assert_eq!(server.gets(&sha), 2);
    assert_eq!(fs::read(cache.blob(&sha)).unwrap(), b"the real clip");

    // Corrupt every time: the sync fails, and neither the cache nor the tree has it.
    server
        .state
        .lock()
        .unwrap()
        .corrupt
        .insert(sha.clone(), 100);
    let cache = store(d.path(), "cache2");
    let dest = d.path().join("two");
    let e = sync_with(&cache, &origin, &lock, &dest).unwrap_err();
    assert!(format!("{e:#}").contains("failed the hash"), "{e:#}");
    assert!(!cache.has(&sha));
    assert!(!dest.exists());
}

#[test]
fn a_flaky_origin_is_retried_and_a_cut_download_resumes_with_a_range() {
    let server = Server::start();
    let d = tempfile::tempdir().unwrap();
    let big = vec![7u8; 10_000];
    let lock = lock_on(
        &server,
        d.path(),
        &[("flaky.mp3", b"flaky"), ("big.mp3", &big)],
    );
    let lock_text = Lock::read(&lock).unwrap();
    let flaky = &lock_text
        .entries
        .iter()
        .find(|e| e.path == "flaky.mp3")
        .unwrap()
        .sha;
    let big_sha = &lock_text
        .entries
        .iter()
        .find(|e| e.path == "big.mp3")
        .unwrap()
        .sha;
    {
        let mut st = server.state.lock().unwrap();
        st.fail.insert(flaky.clone(), 2);
        st.truncate.insert(big_sha.clone(), 1);
    }
    let cache = store(d.path(), "cache");
    let dest = d.path().join("clips");
    sync_with(&cache, &http(&server, None, false), &lock, &dest).unwrap();
    assert_eq!(server.gets(flaky), 3, "two 503s, then the blob");
    assert_eq!(server.state.lock().unwrap().ranges, ["bytes=5000-"]);
    assert_eq!(fs::read(dest.join("big.mp3")).unwrap(), big);
}

#[test]
fn a_bearer_token_is_sent_and_a_missing_one_is_refused() {
    let server = Server::start();
    server.state.lock().unwrap().token = Some("s3cret".into());
    let d = tempfile::tempdir().unwrap();
    let lock = lock_on(&server, d.path(), &[("a.mp3", b"a")]);
    let e = sync_with(
        &store(d.path(), "c1"),
        &http(&server, None, false),
        &lock,
        &d.path().join("one"),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("401"), "{e:#}");
    sync_with(
        &store(d.path(), "c2"),
        &http(&server, Some("s3cret"), false),
        &lock,
        &d.path().join("two"),
    )
    .unwrap();
}

fn pin_with(store: &Store, origin: &dyn Origin, lock: &Path, root: &Path) -> assetsync::PinReport {
    assetsync::pin(
        lock,
        &PinOptions {
            store,
            origin: Some(origin),
            root: root.to_path_buf(),
            paths: vec![],
            merge: false,
            header: Some(vec![("source".into(), "test".into())]),
            workers: 4,
        },
    )
    .unwrap()
}

#[test]
fn an_http_pin_uploads_what_the_origin_lacks_and_another_cache_syncs_it() {
    let server = Server::start();
    let d = tempfile::tempdir().unwrap();
    let already = server.add(b"already there");
    let tree = d.path().join("made");
    fs::create_dir_all(tree.join("w")).unwrap();
    fs::write(tree.join("w/old.mp3"), b"already there").unwrap();
    fs::write(tree.join("w/new.mp3"), b"freshly made").unwrap();
    let lock = d.path().join("voice.lock");
    let origin = http(&server, None, true);
    let r = pin_with(&store(d.path(), "maker"), &origin, &lock, &tree);
    assert_eq!((r.files, r.uploaded, r.uploaded_bytes), (2, 1, 12));
    assert!(server.state.lock().unwrap().blobs.contains_key(&already));

    let dest = d.path().join("elsewhere");
    let r = sync_with(&store(d.path(), "user"), &origin, &lock, &dest).unwrap();
    assert_eq!(r.fetched, 2);
    assert_eq!(fs::read(dest.join("w/new.mp3")).unwrap(), b"freshly made");

    // A read-only origin refuses to be pinned to.
    let e = assetsync::pin(
        &lock,
        &PinOptions {
            store: &store(d.path(), "maker"),
            origin: Some(&http(&server, None, false)),
            root: tree.clone(),
            paths: vec![],
            merge: false,
            header: None,
            workers: 1,
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
}

#[test]
fn the_r2_origin_mints_credentials_lists_by_page_and_signs_each_request() {
    let server = Server::start();
    server.state.lock().unwrap().page = 1;
    server.add(b"one already in the bucket");
    let d = tempfile::tempdir().unwrap();
    let tree = d.path().join("baked");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("a.bin"), b"a").unwrap();
    fs::write(tree.join("b.bin"), b"bb").unwrap();
    let config = |write| R2Config {
        api_base: format!("{}/client/v4", server.base),
        endpoint: Some(server.base.clone()),
        ..R2Config::new("acct", "bucket", "cf-token", write)
    };
    let r2 = R2::connect(config(true), fast(), assetsync::quiet_log()).unwrap();
    assert_eq!(r2.list().unwrap().len(), 1);
    let lock = d.path().join("assets.lock");
    let r = pin_with(&store(d.path(), "maker"), &r2, &lock, &tree);
    assert_eq!(r.uploaded, 2);
    assert_eq!(r2.list().unwrap().len(), 3, "three pages of one");

    let reader = R2::connect(config(false), fast(), assetsync::quiet_log()).unwrap();
    let dest = d.path().join("checkout");
    let r = sync_with(&store(d.path(), "reader"), &reader, &lock, &dest).unwrap();
    assert_eq!(r.fetched, 2);
    assert_eq!(fs::read(dest.join("b.bin")).unwrap(), b"bb");

    let bad = R2Config {
        token: "wrong".into(),
        ..config(false)
    };
    assert!(R2::connect(bad, fast(), assetsync::quiet_log()).is_err());
}
