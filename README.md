# assetsync

Content-addressed asset sync. A **lock file**, committed to git, lists a tree's files by sha256;
the bytes live once each at an **origin** (a Cloudflare R2 bucket, or any HTTP server) and in a
local **cache**; a synced tree is hardlinks into that cache. Large or numerous binary files stay
out of git without Git LFS, every download is verified against the lock, and ten checkouts of the
same assets cost the disk once.

Proprietary to Progression Labs, kept in the private repo
[Progression-Labs/assetsync](https://github.com/Progression-Labs/assetsync) and published only
to its GitHub Packages registry; not open source and not on npmjs.org or crates.io.

It is a Rust crate with a CLI and Node-API bindings (the npm package
`@progression-labs/assetsync`, loadable from Bun or Node), extracted from [zero](https://github.com/ChrisTowles/zero)'s `cargo xtask assets`
and shared by zero (Rust) and [Farwonder](https://github.com/ChrisTowles/learn-app) (Bun
TypeScript). Linux and macOS only: the cache relies on hardlinks, inode numbers and `flock`.

## The lock format

One text file per asset set. Each line is a file:

```
# source voice-clips
# generator 4b1c9e0
1f0e3dad99908345f7439f8ffabdffc4c0e5f2a6b8d7e4c3f2a1b0c9d8e7f6a5 10342 words/cat.mp3
9b74c9897bac770ffc029102a200c5de4c0e5f2a6b8d7e4c3f2a1b0c9d8e7f6a 9876 words/dog house.mp3
```

- `<sha256> <size> <path>`, single spaces; the sha256 is 64 lowercase hex digits, the size is in
  bytes, and the path is the rest of the line (spaces allowed), relative to the tree, `/`-separated.
- Lines are sorted by path (byte order) and each path appears once.
- Lines starting with `#` are header lines, `# <key> <value>`, kept in file order: provenance
  the caller chooses (zero records the importers' revision).
- Blank lines are ignored. A path that is absolute or contains a `..` component is refused: sync
  writes where the lock says, and a lock is data from a commit.

This is zero's `assets.lock` format byte for byte (the `..` check is new); a lock written by one
reads in the other.

## Where the bytes are

- **The origin** holds blob `<sha256>` under a key or URL that is just its hash: R2's
  `<prefix><sha256>` (zero's prefix is `blob/`), or `<base_url>/<sha256>` over HTTP. Content
  addressing means an upload never overwrites anything different and a blob can be cached for ever.
- **The cache** (`$ASSETSYNC_CACHE`, else `$XDG_CACHE_HOME/assetsync`, else `~/.cache/assetsync`)
  keeps blobs at `<ab>/<sha256>`, mode 0444. Downloads land in `tmp/<sha256>.part` and are renamed
  in only after they hash to the lock's sha256 and size. `index.tsv` records when each blob was last
  used and under which path, for `gc`.
- **A synced tree** is hardlinks into the cache, so a file in it is read-only: rewriting one in
  place fails with EACCES, and tools that regenerate assets must delete-then-create or
  write-a-temp-then-rename (which leaves the cached blob untouched). A tree on another filesystem
  gets copies. ext4 allows 65,000 links per inode: past that a blob gets a second copy
  (`<sha256>.1`).
- **Pin** also makes the pinned files themselves the cache's blobs (hardlinked, then 0444), so a
  freshly generated tree costs no extra disk.

## Origins

### R2

A private bucket, reached with a Cloudflare API token that has R2 read (sync) or read-write (pin)
access. Each run verifies the token, exchanges it for temporary S3 credentials scoped to the bucket
and to read or read-write (`/accounts/<id>/r2/temp-access-credentials`, 6 hours), and presigns
each GET and PUT with SigV4. Pin lists the bucket through Cloudflare's API (R2's S3 listing
refuses temporary credentials) to upload only what is missing.

### HTTP

Any server that answers:

| request                   | answer                                                            |
| ------------------------- | ----------------------------------------------------------------- |
| `GET <base_url>/<sha256>` | the blob's bytes (a `Range: bytes=N-` request may get 206 or 200) |
| `GET <base_url>/known`    | for pin: every sha256 it holds, one per line                      |
| `PUT <base_url>/<sha256>` | for pin: store the body as that blob                              |

An optional token is sent as `Authorization: Bearer <token>` on every request. Read-only use
needs only the first route, which suits a public, cacheable, content-addressed route on a Worker
or CDN.

Both origins retry transient failures (connection errors, 5xx, 429) with a doubling wait from
2 s up to a minute, 12 tries in all; a 401 or 403 is tried 3 times (Cloudflare answers a good
token 403 now and then) and any other 4xx fails at once. A download that is cut off resumes with a
Range request. A download that does not hash to the lock's sha256 is thrown away and fetched again,
3 times, and then the sync fails; nothing unverified ever enters the cache.

## CLI

```
cargo install --git https://github.com/Progression-Labs/assetsync assetsync   # needs access to the repo
# or download a release's CLI tarball: gh release download -R Progression-Labs/assetsync
```

```
assetsync pin    <LOCK> [PATHS]... [--root DIR] [--merge] [--header key=value]... [--no-upload]
assetsync sync   <LOCK> [DEST] [--only PREFIX]... [--workers N]
assetsync verify <LOCK> [DIR]
assetsync gc     [--keep LOCK]... [--unused-for 90d]
```

- `pin` hashes `PATHS` (files or directories under `--root`, default the whole root; symlinks and
  this tool's own stamp files are skipped) into the cache, uploads what the origin lacks, and then
  writes the lock. `--merge` keeps the lock's entries outside `PATHS` (adding to a lock pinned on
  several machines); without it the lock is exactly what `PATHS` holds. `--header` replaces the
  header; without one the old header is kept.
- `sync` fetches what the cache lacks (64 parallel downloads) and links every file into `DEST`.
  A destination that does not exist is built in a staging directory beside it and renamed into
  place. In an existing one, files the previous sync of this lock linked and this lock no longer
  names are removed (the list is the `.assetsync-<lock name>` stamp in `DEST`); files the lock never
  named are left alone. `--only` links just the entries under those prefixes and removes nothing.
- `verify` hashes every file against the lock and exits 1 if any is missing or different.
- `gc` lists the cached blobs that none of the `--keep` locks names and nothing has used for
  `--unused-for` (`90d`, `12w`, `3m`), and deletes them only when you type `delete` at a terminal.
  With no terminal it prints the list and refuses; no flag skips the question. It also clears
  download leftovers older than a day.

Global flags: `--cache DIR`, `--quiet`.

Origin flags (pin and sync); tokens are read from the environment or a file, never from a flag:

| origin | flags / environment                                                                                                                                                      |
| ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| HTTP   | `--http URL` / `ASSETSYNC_HTTP_URL`; token `ASSETSYNC_HTTP_TOKEN` or `--http-token-file`                                                                                 |
| R2     | `--r2-account ID` / `ASSETSYNC_R2_ACCOUNT`, `--r2-bucket NAME` / `ASSETSYNC_R2_BUCKET`, `--r2-prefix` (default `blob/`); token `ASSETSYNC_R2_TOKEN` or `--r2-token-file` |

## Rust API

```rust
use assetsync::{Http, HttpConfig, PinOptions, Retry, Store, SyncOptions};

let store = Store::open(assetsync::store::default_root()?)?;
let origin = Http::new(
    HttpConfig { base_url: "https://example.com/blobs".into(), token: None, write: false },
    Retry::default(),
    assetsync::stderr_log(),
);
let report = assetsync::sync(
    "voice.lock".as_ref(),
    "public/voice".as_ref(),
    &SyncOptions { store: &store, origin: Some(&origin), only: vec![], workers: 0 },
)?;
println!("{} fetched, {} linked", report.fetched, report.linked);
```

| item                                                                                     | what                                                                                    |
| ---------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| `Lock::{parse, read, render, write, sort, blobs, header, retain_under}`, `Entry`         | the lock format                                                                         |
| `pin(lock_path, &PinOptions) -> PinReport`                                               | hash, cache, upload, write the lock                                                     |
| `sync(lock_path, dest, &SyncOptions) -> SyncReport`, `sync_lock(&Lock, name, dest, ..)`  | fetch and link                                                                          |
| `verify(lock_path, dir) -> VerifyReport`, `verify_lock`                                  | hash a tree against a lock                                                              |
| `gc::{live_blobs, candidates, review, delete, confirm_and_delete, sweep_tmp, parse_age}` | local gc                                                                                |
| `Store`                                                                                  | the cache: `open`, `blob`, `has_whole`, `ingest`, `put`, `link_into`, `record_use`, ... |
| `Origin` trait; `R2` + `R2Config`, `Http` + `HttpConfig`; `Retry`                        | origins                                                                                 |
| `Log`, `stderr_log()`, `quiet_log()`                                                     | where progress lines go                                                                 |

The library is blocking (threads, not async): the work is file and HTTP I/O on a worker pool.
Errors are `anyhow::Error`, with the path and the sha256 in the context.

## JavaScript API

```ts
import { parseLock, renderLock, pin, sync, verify } from "@progression-labs/assetsync";

const origin = { http: { baseUrl: "https://example.com/clips", token: process.env.CLIPS_TOKEN } };

const r = await sync("voice.lock", "apps/web/public/voice", { origin });
// { files, blobs, fetched, fetchedBytes, linked, already, removed }

const p = await pin("voice.lock", {
  root: "apps/web/public/voice",
  origin,
  header: [{ key: "source", value: "voice:clips" }],
});
// { files, blobs, bytes, hashed, uploaded, uploadedBytes, unavailable }

const v = await verify("voice.lock", "apps/web/public/voice");
// { files, ok, missing: string[], bad: string[] }

const lock = parseLock(text); // { header: { key, value }[], entries: { sha, size, path }[] }
const text2 = renderLock(lock); // sorted, validated
```

`pin`, `sync` and `verify` return Promises and run on the libuv thread pool, never on the JS
thread. Options shared by `pin` and `sync`: `origin` (`{ http: { baseUrl, token? } }` or
`{ r2: { accountId, bucket, token, prefix?, apiBase?, endpoint? } }`), `cacheDir`, `workers`,
`quiet` (no progress lines on stderr) and `retry` (`{ attempts?, baseDelayMs?, maxDelayMs? }`).
`sync` also takes `only`; `pin` takes `root`, `paths`, `merge` and `header`. Failures reject with an
`Error` whose message carries the reason. Types are in [`bindings/node/index.d.ts`](bindings/node/index.d.ts).

The package ships prebuilt binaries as optional dependencies, so no Rust toolchain is needed to
install it: `@progression-labs/assetsync-linux-x64-gnu` (glibc 2.35+), the only platform built
today. macOS (`@progression-labs/assetsync-darwin-arm64`) returns when Progression-Labs has a
macOS runner for private repos: add `aarch64-apple-darwin` back to `napi.targets` and `os`, run
`bun run create-npm-dirs`, and put `macos-14` back in the CI and release matrices. The library
and CLI are written for macOS too, but no macOS build has been tested yet.

### Installing from GitHub Packages

The packages live in GitHub Packages' npm registry, which needs a token even to read. A consumer
adds an `.npmrc` beside its `package.json` (Bun reads it too) that maps the scope to the registry
and takes the token from the environment:

```ini
@progression-labs:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_PACKAGES_TOKEN}
```

`GITHUB_PACKAGES_TOKEN` is a token with `read:packages` and access to Progression-Labs: on a dev
machine, a classic personal access token (or `gh auth token` after `gh auth refresh -s read:packages`);
in a GitHub Actions workflow of another repo, a secret holding such a token (the workflow's own
`GITHUB_TOKEN` reads it only if the package grants that repo access in its settings). The token
is never committed: only the `${...}` reference is. Then:

```
bun add --exact @progression-labs/assetsync
```

```ts
import { sync } from "@progression-labs/assetsync";
```

## How zero and Farwonder use it

**zero** keeps `assets.lock` (the baked tree) and `raw.lock` (machine-only sources) in its R2
bucket's `blob/` prefix. `cargo xtask assets` becomes a thin layer over this crate: it computes
its pin set (the files its baked roots' `.gitignore` files name), refuses trees with a not-a-full-bake
mark, writes the importers' revision as the `importers` header, runs its git hooks, and calls
`assetsync::pin` / `sync` with an `R2` origin and its own cache directory
(`~/.cache/zero/content-addressed-store` keeps working as `Store::open`'s root). `raw.lock`'s
`--add` is `pin` with `merge: true` and the added paths.

**Farwonder** keeps its ~8,000 voice clips (MP3s of ~10 KB) out of git: `voice.lock` lists them,
`bun run voice:sync` calls `sync()` with an HTTP origin pointed at the app's public
content-addressed clip route, and `voice:clips` calls `pin()` after generating new clips, with a
token that lets it upload. The package is pinned exactly in `bun.lock`, and the scripts stay
Bun TypeScript.

## Left out of zero's version

- Compiled-shader sync (`spirv/<key>`), the `.gitignore`-based pin set, the not-a-full-bake marks,
  `adopt` (converting an old full tree into links), the git-LFS pointer checks, the git hooks and
  `git worktree list` discovery for gc (gc takes `--keep` locks instead), and the texture-cache
  pruning: all zero-specific, staying in zero's xtask.
- Hard-coded account id, bucket, token paths and cache dir: configuration now.

New here: the HTTP origin, cross-filesystem copies, the `..` check, a resumed download that a server
answers with 200 restarts the file instead of appending to it, `verify`, gc removes the index rows
of what it deleted and sweeps `tmp/`, and a library API that returns reports instead of printing.

## Building and testing

```
cargo test --workspace                     # unit tests and the origin tests (an in-process HTTP server)
cargo clippy --workspace --all-targets -- -D warnings
cd bindings/node && bun install && bun run build:debug && bun test
```

`cargo test -- --ignored` also runs the hardlink-limit test, which makes 65,001 links.

## Releasing

Bump the version in `Cargo.toml` and `bindings/node/package.json` (and `bindings/node/npm/*/package.json`),
then push a tag `v<version>`. [`release.yml`](.github/workflows/release.yml) builds the addon and the
CLI on Linux x64 (macOS arm64 returns with a runner), runs the smoke test, publishes the two npm packages
to GitHub Packages with the workflow's own `GITHUB_TOKEN` (`packages: write`; no secret to add),
and makes a GitHub release on this repo with the CLI tarballs. Every `package.json` carries
`publishConfig.registry = https://npm.pkg.github.com`, so a publish cannot reach npmjs.org. The
crate is `publish = false`; Rust consumers such as zero take it as a git dependency:

```toml
assetsync = { git = "https://github.com/Progression-Labs/assetsync", tag = "v0.1.0" }
```

## License

Proprietary. Copyright Progression Labs; all rights reserved. Not licensed for use, copying or
distribution outside Progression Labs (`"license": "UNLICENSED"`).
