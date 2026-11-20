//! Where blobs come from and go to. An [`Origin`] lists what it holds, downloads one blob into a
//! `.part` file and uploads one from a file. Two ship: [`R2`] (a Cloudflare R2 bucket, through
//! temporary S3 credentials) and [`Http`] (plain GETs of `<base_url>/<sha256>`).
//!
//! Neither verifies what it downloads: [`crate::sync`] hashes every download before it enters
//! the cache.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

mod http;
mod r2;

pub use http::{Http, HttpConfig};
pub use r2::{R2, R2Config};

use crate::Log;

pub trait Origin: Sync {
    /// Every blob the origin holds: sha -> size (0 where the origin does not say).
    fn list(&self) -> Result<HashMap<String, u64>>;
    /// Downloads blob `sha` of `size` bytes into `part`, resuming what is there.
    fn get(&self, sha: &str, size: u64, part: &Path) -> Result<()>;
    /// Uploads the file `file` (`size` bytes) as blob `sha`.
    fn put(&self, sha: &str, file: &Path, size: u64) -> Result<()>;
    /// True when this origin can [`Origin::put`].
    fn writable(&self) -> bool;
}

/// How transient failures are retried: a doubling wait from `base_delay`, at most `max_delay`,
/// for up to `attempts` tries. A 4xx other than 429 is the caller's mistake and fails at once,
/// but for 401 and 403, tried `auth_attempts` times (Cloudflare answers a good token 403 now and
/// then).
#[derive(Clone, Debug)]
pub struct Retry {
    pub attempts: u32,
    pub auth_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for Retry {
    /// zero's: 12 tries, waiting 2 s, 4 s, ... up to a minute: an outage of a few minutes stalls
    /// a run instead of killing it.
    fn default() -> Retry {
        Retry {
            attempts: 12,
            auth_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl Retry {
    fn wait(&self, attempt: u32) -> Duration {
        self.base_delay
            .saturating_mul(1u32 << attempt.min(16))
            .min(self.max_delay)
    }

    /// Runs `f`, retrying transient failures.
    pub fn run<T>(&self, log: &Log, what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match f() {
                Ok(v) => return Ok(v),
                Err(e)
                    if attempt < self.attempts
                        && transient(&e)
                        && (attempt < self.auth_attempts || !refused(&e)) =>
                {
                    let wait = self.wait(attempt);
                    log(&format!(
                        "{what}: {e:#}; retry {attempt}/{} in {:.1}s",
                        self.attempts,
                        wait.as_secs_f64()
                    ));
                    std::thread::sleep(wait);
                }
                Err(e) => return Err(e.context(what.to_string())),
            }
        }
    }
}

fn refused(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::StatusCode(401 | 403))
    )
}

fn transient(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(c)) => *c == 429 || *c >= 500 || matches!(*c, 401 | 403),
        Some(_) => true,
        None => e.downcast_ref::<std::io::Error>().is_some(),
    }
}

/// The HTTP agent both origins use: a pool as wide as the fetch workers, non-2xx as errors, and
/// time limits on connecting and on the response's head. (A body has no overall limit: a large
/// blob on a slow link takes as long as it takes.)
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(true)
        .max_idle_connections_per_host(64)
        .max_idle_connections(64)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// One attempt at downloading `url` into `part`, resuming from what `part` holds with a Range
/// request. A server that ignores the range (200, not 206) restarts the file.
pub(crate) fn download(
    agent: &ureq::Agent,
    url: &str,
    auth: Option<&str>,
    size: u64,
    part: &Path,
) -> Result<()> {
    let have = part.metadata().map_or(0, |m| m.len());
    if have > size {
        std::fs::remove_file(part)?;
    }
    let have = part.metadata().map_or(0, |m| m.len());
    if have == size && size > 0 {
        return Ok(());
    }
    let mut req = agent.get(url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let mut resp = req.call()?;
    let resumed = resp.status().as_u16() == 206;
    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(part)?;
    std::io::copy(&mut resp.body_mut().as_reader(), &mut out)?;
    out.flush()?;
    if part.metadata().map_or(0, |m| m.len()) < size {
        return Err(
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "download ended short").into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fast() -> Retry {
        Retry {
            attempts: 4,
            auth_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    #[test]
    fn waits_double_up_to_the_cap() {
        let r = Retry::default();
        assert_eq!(r.wait(1), Duration::from_secs(2));
        assert_eq!(r.wait(5), Duration::from_secs(32));
        assert_eq!(r.wait(9), Duration::from_secs(60));
    }

    #[test]
    fn transient_failures_retry_and_mistakes_do_not() {
        let log = crate::quiet_log();
        let n = Cell::new(0);
        let v = fast()
            .run(&log, "x", || {
                n.set(n.get() + 1);
                if n.get() < 3 {
                    Err(ureq::Error::StatusCode(503).into())
                } else {
                    Ok(7)
                }
            })
            .unwrap();
        assert_eq!((v, n.get()), (7, 3));

        n.set(0);
        let e = fast()
            .run(&log, "x", || -> Result<()> {
                n.set(n.get() + 1);
                Err(ureq::Error::StatusCode(404).into())
            })
            .unwrap_err();
        assert_eq!(n.get(), 1);
        assert!(format!("{e:#}").starts_with("x: "));

        n.set(0);
        let _ = fast().run(&log, "x", || -> Result<()> {
            n.set(n.get() + 1);
            Err(ureq::Error::StatusCode(403).into())
        });
        assert_eq!(n.get(), 2, "a refusal is tried auth_attempts times");
    }
}
