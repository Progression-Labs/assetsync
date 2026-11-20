//! A plain HTTP origin: `GET <base_url>/<sha256>` is the blob (a public, content-addressed
//! route, like Farwonder's clips). With a token every request carries `Authorization: Bearer`.
//! Uploads, where the server takes them, are `PUT <base_url>/<sha256>` with the bytes as the body,
//! and what the server already holds is `GET <base_url>/known`: one sha256 per line.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Result, bail};

use super::{Origin, Retry, agent, download};
use crate::Log;

#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// The blobs' base, without a trailing slash: `https://example.com/blobs`.
    pub base_url: String,
    /// Sent as `Authorization: Bearer <token>`. Never logged.
    pub token: Option<String>,
    /// Whether `put` and `list` may be used (the server takes uploads and serves `/known`).
    pub write: bool,
}

pub struct Http {
    agent: ureq::Agent,
    base: String,
    auth: Option<String>,
    write: bool,
    retry: Retry,
    log: Log,
}

impl Http {
    pub fn new(config: HttpConfig, retry: Retry, log: Log) -> Http {
        Http {
            agent: agent(),
            base: config.base_url.trim_end_matches('/').to_string(),
            auth: config.token.map(|t| format!("Bearer {}", t.trim())),
            write: config.write,
            retry,
            log,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    fn with_auth<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match &self.auth {
            Some(a) => req.header("Authorization", a),
            None => req,
        }
    }
}

impl Origin for Http {
    fn list(&self) -> Result<HashMap<String, u64>> {
        let url = self.url("known");
        let text = self
            .retry
            .run(&self.log, "listing the origin's blobs", || {
                Ok(self
                    .with_auth(self.agent.get(&url))
                    .call()?
                    .body_mut()
                    .with_config()
                    .limit(1 << 30)
                    .read_to_string()?)
            })?;
        let mut out = HashMap::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !crate::lock::is_sha(line) {
                bail!("{url}: `{line}` is not a lowercase sha256");
            }
            out.insert(line.to_string(), 0);
        }
        Ok(out)
    }

    fn get(&self, sha: &str, size: u64, part: &Path) -> Result<()> {
        let url = self.url(sha);
        self.retry.run(&self.log, &format!("fetch {sha}"), || {
            download(&self.agent, &url, self.auth.as_deref(), size, part)
        })
    }

    fn put(&self, sha: &str, file: &Path, size: u64) -> Result<()> {
        if !self.write {
            bail!("this HTTP origin is read-only (no upload configured)");
        }
        let url = self.url(sha);
        let f = File::open(file)?;
        self.retry.run(&self.log, &format!("upload {sha}"), || {
            let mut f = f.try_clone()?;
            f.seek(SeekFrom::Start(0))?;
            self.with_auth(self.agent.put(&url))
                .header("Content-Length", size.to_string())
                .header("Content-Type", "application/octet-stream")
                .send(ureq::SendBody::from_reader(&mut f.take(size)))?;
            Ok(())
        })
    }

    fn writable(&self) -> bool {
        self.write
    }
}
