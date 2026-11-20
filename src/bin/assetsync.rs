//! `assetsync`: pin, sync, verify and gc from the shell. Tokens come from the environment or a
//! file, never a flag (a flag shows in `ps`).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use assetsync::{
    Http, HttpConfig, Log, Origin, PinOptions, R2, R2Config, Retry, Store, SyncOptions, gc,
};

#[derive(Parser)]
#[command(
    version,
    about = "Content-addressed asset sync: a sha256 lock, an origin, a local cache"
)]
struct Cli {
    /// The local store. Default: $ASSETSYNC_CACHE, else $XDG_CACHE_HOME/assetsync, else
    /// ~/.cache/assetsync.
    #[arg(long, global = true)]
    cache: Option<PathBuf>,
    /// No progress lines.
    #[arg(long, short, global = true)]
    quiet: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hash files into the cache, upload what the origin lacks, and rewrite the lock.
    Pin {
        /// The lock to write.
        lock: PathBuf,
        /// Files or directories to pin, relative to --root. None: the whole root.
        paths: Vec<String>,
        /// The tree the lock's paths are relative to.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Keep the lock's entries outside PATHS (add to the lock rather than replace it).
        #[arg(long)]
        merge: bool,
        /// A `# key value` header line, as key=value (repeatable). Replaces the old header;
        /// without any the old one is kept.
        #[arg(long = "header", value_parser = parse_header)]
        headers: Vec<(String, String)>,
        /// Pin into the cache and the lock only; upload nothing.
        #[arg(long)]
        no_upload: bool,
        #[command(flatten)]
        origin: OriginArgs,
    },
    /// Fetch the blobs the lock names that the cache lacks, and link them into DEST.
    Sync {
        lock: PathBuf,
        /// The tree to make match the lock.
        #[arg(default_value = ".")]
        dest: PathBuf,
        /// Only entries under these path prefixes (repeatable); nothing is removed.
        #[arg(long)]
        only: Vec<String>,
        /// Parallel downloads.
        #[arg(long, default_value_t = assetsync::sync::FETCH_WORKERS)]
        workers: usize,
        #[command(flatten)]
        origin: OriginArgs,
    },
    /// Hash DIR's files against the lock. Exits 1 when any is missing or different.
    Verify {
        lock: PathBuf,
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Review the cached blobs none of the given locks names and nothing used in a while, and
    /// delete them only when you confirm at the terminal.
    Gc {
        /// A lock whose blobs stay (repeatable): every checkout's and worktree's.
        #[arg(long = "keep")]
        keep: Vec<PathBuf>,
        /// How long a blob must have gone unused: `90d`, `12w`, `3m` (30 days).
        #[arg(long, default_value = "90d", value_parser = gc::parse_age)]
        unused_for: u64,
    },
}

#[derive(Args)]
struct OriginArgs {
    /// An HTTP origin: blobs at <URL>/<sha256>. Token: $ASSETSYNC_HTTP_TOKEN or --http-token-file.
    #[arg(long, env = "ASSETSYNC_HTTP_URL", conflicts_with = "r2_bucket")]
    http: Option<String>,
    #[arg(long)]
    http_token_file: Option<PathBuf>,
    /// An R2 origin: the Cloudflare account id.
    #[arg(long, env = "ASSETSYNC_R2_ACCOUNT", requires = "r2_bucket")]
    r2_account: Option<String>,
    /// The R2 bucket. Token: $ASSETSYNC_R2_TOKEN or --r2-token-file.
    #[arg(long, env = "ASSETSYNC_R2_BUCKET", requires = "r2_account")]
    r2_bucket: Option<String>,
    /// The key prefix of a blob in the bucket.
    #[arg(long, env = "ASSETSYNC_R2_PREFIX", default_value = "blob/")]
    r2_prefix: String,
    #[arg(long)]
    r2_token_file: Option<PathBuf>,
}

fn parse_header(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}`: expected key=value"))?;
    if k.is_empty() || k.contains(char::is_whitespace) {
        return Err(format!("`{k}`: a header key is one word"));
    }
    Ok((k.to_string(), v.to_string()))
}

/// A token from `var`, else `file`; `None` when neither is set.
fn token(var: &str, file: Option<&Path>) -> Result<Option<String>> {
    if let Ok(t) = std::env::var(var)
        && !t.trim().is_empty()
    {
        return Ok(Some(t.trim().to_string()));
    }
    match file {
        Some(f) => Ok(Some(
            std::fs::read_to_string(f)
                .with_context(|| format!("reading the token file {}", f.display()))?
                .trim()
                .to_string(),
        )),
        None => Ok(None),
    }
}

impl OriginArgs {
    fn open(&self, write: bool, log: &Log) -> Result<Option<Box<dyn Origin>>> {
        if let Some(url) = &self.http {
            let token = token("ASSETSYNC_HTTP_TOKEN", self.http_token_file.as_deref())?;
            return Ok(Some(Box::new(Http::new(
                HttpConfig {
                    base_url: url.clone(),
                    token,
                    write,
                },
                Retry::default(),
                log.clone(),
            ))));
        }
        if let (Some(account), Some(bucket)) = (&self.r2_account, &self.r2_bucket) {
            let token = token("ASSETSYNC_R2_TOKEN", self.r2_token_file.as_deref())?
                .context("an R2 origin needs a Cloudflare API token with R2 access: set $ASSETSYNC_R2_TOKEN or pass --r2-token-file")?;
            let mut config = R2Config::new(account, bucket, &token, write);
            config.prefix = self.r2_prefix.clone();
            return Ok(Some(Box::new(R2::connect(
                config,
                Retry::default(),
                log.clone(),
            )?)));
        }
        Ok(None)
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let log = if cli.quiet {
        assetsync::quiet_log()
    } else {
        assetsync::stderr_log()
    };
    let root = match cli.cache {
        Some(c) => c,
        None => assetsync::store::default_root()?,
    };
    let store = Store::open_with(root, log.clone())?;
    match cli.cmd {
        Cmd::Pin {
            lock,
            paths,
            root,
            merge,
            headers,
            no_upload,
            origin,
        } => {
            let origin = if no_upload {
                None
            } else {
                match origin.open(true, &log)? {
                    Some(o) => Some(o),
                    None => bail!(
                        "pin uploads to an origin: give --http or --r2-account/--r2-bucket, or --no-upload"
                    ),
                }
            };
            let r = assetsync::pin(
                &lock,
                &PinOptions {
                    store: &store,
                    origin: origin.as_deref(),
                    root,
                    paths,
                    merge,
                    header: (!headers.is_empty()).then_some(headers),
                    workers: 0,
                },
            )?;
            if !r.unavailable.is_empty() {
                eprintln!(
                    "assetsync: {} blobs are at neither the origin nor this cache",
                    r.unavailable.len()
                );
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Sync {
            lock,
            dest,
            only,
            workers,
            origin,
        } => {
            let origin = origin.open(false, &log)?;
            assetsync::sync(
                &lock,
                &dest,
                &SyncOptions {
                    store: &store,
                    origin: origin.as_deref(),
                    only,
                    workers,
                },
            )?;
        }
        Cmd::Verify { lock, dir } => {
            let r = assetsync::verify(&lock, &dir)?;
            for p in &r.missing {
                println!("missing {p}");
            }
            for p in &r.bad {
                println!("differs {p}");
            }
            println!("{} of {} files match the lock", r.ok, r.files);
            if !r.is_ok() {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Gc { keep, unused_for } => {
            let live = gc::live_blobs(&keep)?;
            let c = gc::candidates(&store, &live, unused_for, assetsync::store::now())?;
            let mut out = std::io::stdout();
            gc::review(&c, &mut out)?;
            let stdin = std::io::stdin();
            let n = gc::confirm_and_delete(
                &store,
                &c,
                stdin.is_terminal(),
                &mut stdin.lock(),
                &mut out,
            )?;
            if n > 0 {
                println!("deleted {n} blobs");
            }
            let swept = gc::sweep_tmp(&store, assetsync::store::now())?;
            if swept > 0 {
                println!("removed {swept} download leftovers");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("assetsync: {e:#}");
            ExitCode::FAILURE
        }
    }
}
