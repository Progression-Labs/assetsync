//! Content-addressed asset sync.
//!
//! A **lock** (committed to git) lists a tree's files as `<sha256> <size> <path>` lines, sorted
//! by path. The bytes live once each, named by their sha256, at an **origin** (an R2 bucket's
//! `blob/<sha256>`, or any HTTP server's `<base_url>/<sha256>`) and in a local **store**
//! (`~/.cache/assetsync/<ab>/<sha256>`, read-only); a synced tree is hardlinks into the store.
//!
//! - [`pin()`] hashes a tree into the store, uploads what the origin lacks, rewrites the lock;
//! - [`sync()`] fetches what the store lacks (parallel, resumable, every download hashed) and
//!   links the lock's files into a destination, removing what an earlier sync of it left;
//! - [`verify()`] hashes a tree against its lock;
//! - [`gc`] finds the store's blobs no lock names that went unused for months.
//!
//! Unix only (Linux, macOS): the store relies on hardlinks, inode numbers and `flock`.

use std::sync::Arc;

pub mod gc;
pub mod lock;
pub mod origin;
mod par;
pub mod pin;
pub mod store;
pub mod sync;
pub mod verify;

pub use lock::{Entry, Lock};
pub use origin::{Http, HttpConfig, Origin, R2, R2Config, Retry};
pub use pin::{PinOptions, PinReport, pin};
pub use store::Store;
pub use sync::{SyncOptions, SyncReport, sync, sync_lock};
pub use verify::{VerifyReport, verify, verify_lock};

/// Where progress and retry lines go: one line per call, no trailing newline.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// Lines to stderr, prefixed `assetsync: `.
pub fn stderr_log() -> Log {
    Arc::new(|line| eprintln!("assetsync: {line}"))
}

/// No lines.
pub fn quiet_log() -> Log {
    Arc::new(|_| {})
}
