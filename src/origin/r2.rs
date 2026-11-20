//! A Cloudflare R2 origin: a bucket holding every blob at `<prefix><sha256>` (zero's is
//! `blob/`). It takes a Cloudflare API token with R2 access (never logged), exchanges it once per
//! run for temporary S3 credentials scoped to the bucket and to read or read-write, and signs
//! each GET and PUT with those (SigV4, presigned URLs). Listing goes through Cloudflare's API:
//! R2's S3 listing refuses temporary credentials.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusty_s3::actions::{GetObject, PutObject};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde_json::Value;

use super::{Origin, Retry, agent, download};
use crate::Log;

pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
const SIGNED_FOR: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug)]
pub struct R2Config {
    pub account_id: String,
    pub bucket: String,
    /// A Cloudflare API token with R2 read (or, with `write`, write) on the bucket. Never logged.
    pub token: String,
    /// Mint read-write credentials instead of read-only.
    pub write: bool,
    /// The key prefix of a blob: `blob/` puts blob `<sha>` at `blob/<sha>`.
    pub prefix: String,
    /// Cloudflare's API ([`CLOUDFLARE_API`]); a test points it at a fake.
    pub api_base: String,
    /// The S3 endpoint; `https://<account_id>.r2.cloudflarestorage.com` when `None`.
    pub endpoint: Option<String>,
}

impl R2Config {
    /// The usual config: Cloudflare's API and R2's endpoint, blobs under `blob/`.
    pub fn new(account_id: &str, bucket: &str, token: &str, write: bool) -> R2Config {
        R2Config {
            account_id: account_id.to_string(),
            bucket: bucket.to_string(),
            token: token.trim().to_string(),
            write,
            prefix: "blob/".to_string(),
            api_base: CLOUDFLARE_API.to_string(),
            endpoint: None,
        }
    }
}

pub struct R2 {
    agent: ureq::Agent,
    bucket: Bucket,
    creds: Credentials,
    auth: String,
    config: R2Config,
    retry: Retry,
    log: Log,
}

fn json(mut r: ureq::http::Response<ureq::Body>) -> Result<Value> {
    Ok(serde_json::from_str(&r.body_mut().read_to_string()?)?)
}

impl R2 {
    /// Verifies the token and mints temporary credentials.
    pub fn connect(config: R2Config, retry: Retry, log: Log) -> Result<R2> {
        let agent = agent();
        let auth = format!("Bearer {}", config.token);
        let api = config.api_base.trim_end_matches('/');
        let account = &config.account_id;
        // The token's id is the parent of the temporary credentials.
        let verify = |url: String| -> Result<Value> {
            // One try each: a 401 here means "the other kind of token", not a flake.
            json(agent.get(&url).header("Authorization", &auth).call()?)
        };
        let id = verify(format!("{api}/accounts/{account}/tokens/verify"))
            .or_else(|_| verify(format!("{api}/user/tokens/verify")))
            .context("the R2 token was refused by Cloudflare")?["result"]["id"]
            .as_str()
            .context("token verify gave no id")?
            .to_string();
        let body = serde_json::json!({
            "bucket": config.bucket,
            "parentAccessKeyId": id,
            "permission": if config.write { "object-read-write" } else { "object-read-only" },
            "ttlSeconds": SIGNED_FOR.as_secs(),
        });
        let v = retry.run(
            &log,
            "minting temporary R2 credentials (does the token have R2 access?)",
            || {
                let resp = agent
                    .post(&format!("{api}/accounts/{account}/r2/temp-access-credentials"))
                    .header("Authorization", &auth)
                    .header("Content-Type", "application/json")
                    .send(body.to_string())?;
                json(resp)
            },
        )?;
        let r = &v["result"];
        let field = |k: &str| -> Result<String> {
            Ok(r[k]
                .as_str()
                .with_context(|| format!("temp credentials lack {k}"))?
                .to_string())
        };
        let creds = Credentials::new_with_token(
            field("accessKeyId")?,
            field("secretAccessKey")?,
            field("sessionToken")?,
        );
        let endpoint = config
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{account}.r2.cloudflarestorage.com"))
            .parse()?;
        let bucket = Bucket::new(endpoint, UrlStyle::Path, config.bucket.clone(), "auto")?;
        Ok(R2 {
            agent,
            bucket,
            creds,
            auth,
            config,
            retry,
            log,
        })
    }

    fn key(&self, sha: &str) -> String {
        format!("{}{sha}", self.config.prefix)
    }
}

impl Origin for R2 {
    fn list(&self) -> Result<HashMap<String, u64>> {
        let prefix = &self.config.prefix;
        let url = format!(
            "{}/accounts/{}/r2/buckets/{}/objects",
            self.config.api_base.trim_end_matches('/'),
            self.config.account_id,
            self.config.bucket
        );
        let mut out = HashMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let v = self
                .retry
                .run(&self.log, &format!("listing {}", self.config.bucket), || {
                    let mut req = self
                        .agent
                        .get(&url)
                        .query("prefix", prefix)
                        .query("per_page", "1000")
                        .header("Authorization", &self.auth);
                    if let Some(c) = &cursor {
                        req = req.query("cursor", c);
                    }
                    json(req.call()?)
                })?;
            for o in v["result"].as_array().map_or(&[][..], |a| &a[..]) {
                if let Some(sha) = o["key"].as_str().and_then(|k| k.strip_prefix(prefix.as_str()))
                {
                    out.insert(sha.to_string(), o["size"].as_u64().unwrap_or(0));
                }
            }
            match v["result_info"]["cursor"]
                .as_str()
                .filter(|c| !c.is_empty())
            {
                Some(c) => cursor = Some(c.to_string()),
                None => return Ok(out),
            }
        }
    }

    fn get(&self, sha: &str, size: u64, part: &Path) -> Result<()> {
        let url = GetObject::new(&self.bucket, Some(&self.creds), &self.key(sha)).sign(SIGNED_FOR);
        self.retry.run(&self.log, &format!("fetch {sha}"), || {
            download(&self.agent, url.as_str(), None, size, part)
        })
    }

    fn put(&self, sha: &str, file: &Path, size: u64) -> Result<()> {
        if !self.config.write {
            bail!("this R2 origin was opened read-only");
        }
        let url = PutObject::new(&self.bucket, Some(&self.creds), &self.key(sha)).sign(SIGNED_FOR);
        let f = File::open(file)?;
        self.retry.run(&self.log, &format!("upload {sha}"), || {
            let mut f = f.try_clone()?;
            f.seek(SeekFrom::Start(0))?;
            self.agent
                .put(url.as_str())
                .header("Content-Length", size.to_string())
                .send(ureq::SendBody::from_reader(&mut f.take(size)))?;
            Ok(())
        })
    }

    fn writable(&self) -> bool {
        self.config.write
    }
}
