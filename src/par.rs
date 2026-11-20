//! A small worker pool and a progress line, without an async runtime: the work is blocking file
//! and HTTP I/O, and a thread per worker is what it needs.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;

use crate::Log;

/// The workers for local work (hashing, linking): the machine's cores, 4 to 16.
pub fn local_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(8, |n| n.get())
        .clamp(4, 16)
}

/// Runs `f` over `items` on `threads` workers; the first error stops the rest and is returned.
pub fn par<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    let error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len()).max(1) {
            s.spawn(|| {
                while !failed.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    match f(item) {
                        Ok(r) => out.lock().unwrap()[i] = Some(r),
                        Err(e) => {
                            failed.store(true, Ordering::Relaxed);
                            error.lock().unwrap().get_or_insert(e);
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = error.into_inner().unwrap() {
        return Err(e);
    }
    Ok(out.into_inner().unwrap().into_iter().flatten().collect())
}

/// A progress line every few seconds from whichever worker gets there.
pub struct Progress<'a> {
    what: &'static str,
    total: usize,
    done: AtomicUsize,
    last: Mutex<Instant>,
    log: &'a Log,
}

impl<'a> Progress<'a> {
    pub fn new(log: &'a Log, what: &'static str, total: usize) -> Progress<'a> {
        Progress {
            what,
            total,
            done: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
            log,
        }
    }

    pub fn tick(&self) {
        let n = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self.last.lock().unwrap();
        if last.elapsed().as_secs() >= 5 {
            *last = Instant::now();
            (self.log)(&format!("{} {n}/{}", self.what, self.total));
        }
    }
}

pub fn mb(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}
