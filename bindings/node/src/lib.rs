//! Node-API bindings: the npm package `assetsync`. `parseLock` and `renderLock` run on the JS
//! thread (pure string work); `pin`, `sync` and `verify` return Promises and run on libuv's
//! thread pool, so file and network I/O never block the event loop.
//!
//! Sizes and byte counts are JS numbers (f64): exact to 2^53 bytes, which is 9 PB.

use std::path::PathBuf;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

fn err(e: anyhow::Error) -> Error {
    Error::from_reason(format!("{e:#}"))
}

#[napi(object)]
#[derive(Clone)]
pub struct LockEntry {
    /// Lowercase hex sha256.
    pub sha: String,
    pub size: f64,
    pub path: String,
}

#[napi(object)]
#[derive(Clone)]
pub struct LockHeader {
    pub key: String,
    pub value: String,
}

#[napi(object)]
pub struct Lock {
    /// `# key value` header lines, in file order.
    pub header: Vec<LockHeader>,
    /// Sorted by path.
    pub entries: Vec<LockEntry>,
}

/// Plain GETs of `<baseUrl>/<sha256>`; uploads are `PUT <baseUrl>/<sha256>` and what the server
/// holds is `GET <baseUrl>/known` (one sha256 per line).
#[napi(object)]
#[derive(Clone)]
pub struct HttpOrigin {
    pub base_url: String,
    /// Sent as `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

/// A Cloudflare R2 bucket, through temporary S3 credentials minted from an API token.
#[napi(object)]
#[derive(Clone)]
pub struct R2Origin {
    pub account_id: String,
    pub bucket: String,
    /// A Cloudflare API token with R2 access to the bucket.
    pub token: String,
    /// A blob's key prefix; `blob/` by default.
    pub prefix: Option<String>,
    /// Cloudflare's API base, for a test's fake.
    pub api_base: Option<String>,
    /// The S3 endpoint; `https://<accountId>.r2.cloudflarestorage.com` by default.
    pub endpoint: Option<String>,
}

/// One of the two.
#[napi(object)]
#[derive(Clone)]
pub struct OriginOptions {
    pub http: Option<HttpOrigin>,
    pub r2: Option<R2Origin>,
}

/// Transient failures (network, 5xx, 429) are retried with a doubling wait.
#[napi(object)]
#[derive(Clone)]
pub struct RetryOptions {
    /// Tries in all; 12 by default.
    pub attempts: Option<u32>,
    /// The first wait; 2000 ms by default, doubling each try.
    pub base_delay_ms: Option<u32>,
    /// The longest wait; 60000 ms by default.
    pub max_delay_ms: Option<u32>,
}

#[napi(object)]
#[derive(Clone)]
pub struct SyncOptions {
    /// Where blobs the cache lacks come from.
    pub origin: Option<OriginOptions>,
    /// The local store; `$ASSETSYNC_CACHE`, `$XDG_CACHE_HOME/assetsync` or `~/.cache/assetsync`.
    pub cache_dir: Option<String>,
    /// Only entries under these path prefixes; nothing is removed.
    pub only: Option<Vec<String>>,
    /// Parallel downloads; 64 by default.
    pub workers: Option<u32>,
    /// No progress lines on stderr.
    pub quiet: Option<bool>,
    pub retry: Option<RetryOptions>,
}

#[napi(object)]
pub struct SyncResult {
    /// Entries synced (after `only`), and distinct blobs among them.
    pub files: u32,
    pub blobs: u32,
    /// Blobs downloaded, and their bytes.
    pub fetched: u32,
    pub fetched_bytes: f64,
    /// Files (re)linked, already in place, and removed (named by an earlier sync, not this lock).
    pub linked: u32,
    pub already: u32,
    pub removed: u32,
}

#[napi(object)]
#[derive(Clone)]
pub struct PinOptions {
    /// The tree the lock's paths are relative to.
    pub root: String,
    /// Files or directories under `root` to pin; the whole root when absent or empty.
    pub paths: Option<Vec<String>>,
    /// Keep the old lock's entries outside `paths`.
    pub merge: Option<bool>,
    /// The lock's header; the old lock's is kept when absent.
    pub header: Option<Vec<LockHeader>>,
    /// Where to upload what it lacks; nothing is uploaded when absent.
    pub origin: Option<OriginOptions>,
    pub cache_dir: Option<String>,
    /// Parallel uploads; 12 by default.
    pub workers: Option<u32>,
    pub quiet: Option<bool>,
    pub retry: Option<RetryOptions>,
}

#[napi(object)]
pub struct PinResult {
    /// Entries in the written lock, distinct blobs, and their bytes.
    pub files: u32,
    pub blobs: u32,
    pub bytes: f64,
    /// Files read and hashed this run (one already linked to its blob is not).
    pub hashed: u32,
    /// Blobs uploaded, and their bytes.
    pub uploaded: u32,
    pub uploaded_bytes: f64,
    /// sha256s the lock names that neither the origin nor this cache has.
    pub unavailable: Vec<String>,
}

#[napi(object)]
pub struct VerifyResult {
    pub files: u32,
    pub ok: u32,
    /// Paths not in the tree.
    pub missing: Vec<String>,
    /// Paths whose size or sha256 differs from the lock.
    pub bad: Vec<String>,
}

fn to_js_lock(lock: assetsync::Lock) -> Lock {
    Lock {
        header: lock
            .header
            .into_iter()
            .map(|(key, value)| LockHeader { key, value })
            .collect(),
        entries: lock
            .entries
            .into_iter()
            .map(|e| LockEntry {
                sha: e.sha,
                size: e.size as f64,
                path: e.path,
            })
            .collect(),
    }
}

/// Parses a lock's text. Throws on a malformed line, an unsorted or repeated path, or a path
/// outside the tree.
#[napi]
pub fn parse_lock(text: String) -> Result<Lock> {
    assetsync::Lock::parse(&text).map(to_js_lock).map_err(err)
}

/// A lock's text, entries sorted by path. Throws on an entry `parseLock` would refuse.
#[napi]
pub fn render_lock(lock: Lock) -> Result<String> {
    let mut out = assetsync::Lock {
        header: lock.header.into_iter().map(|h| (h.key, h.value)).collect(),
        entries: Vec::with_capacity(lock.entries.len()),
    };
    for e in lock.entries {
        if !(e.size >= 0.0 && e.size.fract() == 0.0) {
            return Err(Error::from_reason(format!(
                "{}: size {} is not a byte count",
                e.path, e.size
            )));
        }
        out.entries.push(assetsync::Entry {
            sha: e.sha,
            size: e.size as u64,
            path: e.path,
        });
    }
    out.sort();
    let text = out.render();
    // The same checks a reader makes.
    assetsync::Lock::parse(&text).map_err(err)?;
    Ok(text)
}

fn log(quiet: Option<bool>) -> assetsync::Log {
    if quiet.unwrap_or(false) {
        assetsync::quiet_log()
    } else {
        assetsync::stderr_log()
    }
}

fn retry(r: Option<&RetryOptions>) -> assetsync::Retry {
    let mut out = assetsync::Retry::default();
    if let Some(r) = r {
        if let Some(a) = r.attempts {
            out.attempts = a.max(1);
        }
        if let Some(ms) = r.base_delay_ms {
            // The first wait is twice the base.
            out.base_delay = Duration::from_millis(u64::from(ms) / 2);
        }
        if let Some(ms) = r.max_delay_ms {
            out.max_delay = Duration::from_millis(u64::from(ms));
        }
    }
    out
}

fn store(cache_dir: Option<&str>, log: assetsync::Log) -> anyhow::Result<assetsync::Store> {
    let root = match cache_dir {
        Some(d) => PathBuf::from(d),
        None => assetsync::store::default_root()?,
    };
    assetsync::Store::open_with(root, log)
}

fn origin(
    o: Option<&OriginOptions>,
    write: bool,
    retry: assetsync::Retry,
    log: &assetsync::Log,
) -> anyhow::Result<Option<Box<dyn assetsync::Origin>>> {
    let Some(o) = o else { return Ok(None) };
    match (&o.http, &o.r2) {
        (Some(_), Some(_)) => anyhow::bail!("origin: give `http` or `r2`, not both"),
        (Some(h), None) => Ok(Some(Box::new(assetsync::Http::new(
            assetsync::HttpConfig {
                base_url: h.base_url.clone(),
                token: h.token.clone(),
                write,
            },
            retry,
            log.clone(),
        )))),
        (None, Some(r)) => {
            let mut c = assetsync::R2Config::new(&r.account_id, &r.bucket, &r.token, write);
            if let Some(p) = &r.prefix {
                c.prefix = p.clone();
            }
            if let Some(a) = &r.api_base {
                c.api_base = a.clone();
            }
            c.endpoint = r.endpoint.clone();
            Ok(Some(Box::new(assetsync::R2::connect(
                c,
                retry,
                log.clone(),
            )?)))
        }
        (None, None) => Ok(None),
    }
}

pub struct SyncTask {
    lock: String,
    dest: String,
    opts: SyncOptions,
}

impl Task for SyncTask {
    type Output = assetsync::SyncReport;
    type JsValue = SyncResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let o = &self.opts;
        let run = || -> anyhow::Result<assetsync::SyncReport> {
            let log = log(o.quiet);
            let store = store(o.cache_dir.as_deref(), log.clone())?;
            let origin = origin(o.origin.as_ref(), false, retry(o.retry.as_ref()), &log)?;
            assetsync::sync(
                self.lock.as_ref(),
                self.dest.as_ref(),
                &assetsync::SyncOptions {
                    store: &store,
                    origin: origin.as_deref(),
                    only: o.only.clone().unwrap_or_default(),
                    workers: o.workers.unwrap_or(0) as usize,
                },
            )
        };
        run().map_err(err)
    }

    fn resolve(&mut self, _env: Env, r: Self::Output) -> Result<Self::JsValue> {
        Ok(SyncResult {
            files: r.files as u32,
            blobs: r.blobs as u32,
            fetched: r.fetched as u32,
            fetched_bytes: r.fetched_bytes as f64,
            linked: r.linked as u32,
            already: r.already as u32,
            removed: r.removed as u32,
        })
    }
}

/// Fetches the blobs the lock at `lock` names that the cache lacks (every download hashed
/// before it is kept), and links the lock's files into `dest`. A whole-lock sync removes what an
/// earlier sync of this lock linked there that it no longer names.
#[napi(ts_return_type = "Promise<SyncResult>")]
pub fn sync(lock: String, dest: String, opts: Option<SyncOptions>) -> AsyncTask<SyncTask> {
    AsyncTask::new(SyncTask {
        lock,
        dest,
        opts: opts.unwrap_or(SyncOptions {
            origin: None,
            cache_dir: None,
            only: None,
            workers: None,
            quiet: None,
            retry: None,
        }),
    })
}

pub struct PinTask {
    lock: String,
    opts: PinOptions,
}

impl Task for PinTask {
    type Output = assetsync::PinReport;
    type JsValue = PinResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let o = &self.opts;
        let run = || -> anyhow::Result<assetsync::PinReport> {
            let log = log(o.quiet);
            let store = store(o.cache_dir.as_deref(), log.clone())?;
            let origin = origin(o.origin.as_ref(), true, retry(o.retry.as_ref()), &log)?;
            assetsync::pin(
                self.lock.as_ref(),
                &assetsync::PinOptions {
                    store: &store,
                    origin: origin.as_deref(),
                    root: PathBuf::from(&o.root),
                    paths: o.paths.clone().unwrap_or_default(),
                    merge: o.merge.unwrap_or(false),
                    header: o
                        .header
                        .as_ref()
                        .map(|h| h.iter().map(|h| (h.key.clone(), h.value.clone())).collect()),
                    workers: o.workers.unwrap_or(0) as usize,
                },
            )
        };
        run().map_err(err)
    }

    fn resolve(&mut self, _env: Env, r: Self::Output) -> Result<Self::JsValue> {
        Ok(PinResult {
            files: r.files as u32,
            blobs: r.blobs as u32,
            bytes: r.bytes as f64,
            hashed: r.hashed as u32,
            uploaded: r.uploaded as u32,
            uploaded_bytes: r.uploaded_bytes as f64,
            unavailable: r.unavailable,
        })
    }
}

/// Hashes the files under `opts.root` into the cache, uploads what the origin lacks, and writes
/// the lock at `lock` (last, so a lock never names what the origin has not got).
#[napi(ts_return_type = "Promise<PinResult>")]
pub fn pin(lock: String, opts: PinOptions) -> AsyncTask<PinTask> {
    AsyncTask::new(PinTask { lock, opts })
}

pub struct VerifyTask {
    lock: String,
    dir: String,
}

impl Task for VerifyTask {
    type Output = assetsync::VerifyReport;
    type JsValue = VerifyResult;

    fn compute(&mut self) -> Result<Self::Output> {
        assetsync::verify(self.lock.as_ref(), self.dir.as_ref()).map_err(err)
    }

    fn resolve(&mut self, _env: Env, r: Self::Output) -> Result<Self::JsValue> {
        Ok(VerifyResult {
            files: r.files as u32,
            ok: r.ok as u32,
            missing: r.missing,
            bad: r.bad,
        })
    }
}

/// Hashes the files under `dir` against the lock at `lock`; changes nothing.
#[napi(ts_return_type = "Promise<VerifyResult>")]
pub fn verify(lock: String, dir: String) -> AsyncTask<VerifyTask> {
    AsyncTask::new(VerifyTask { lock, dir })
}
