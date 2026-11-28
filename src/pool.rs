//! Shared helpers for upscaling: a pool of large byte buffers that are used
//! again instead of being allocated anew (a fresh multi-megabyte allocation
//! costs a page fault per 4 KB the first time it is written, several times
//! the cost of the upscale itself), and a few worker threads that split one
//! large texture.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

// ---------------------------------------------------------------- buffers

/// Buffers smaller than this are cheap to allocate and not kept.
const KEEP_MIN: usize = 256 << 10;
/// Larger buffers than this are not kept either (rare, would pin memory).
const KEEP_MAX: usize = 32 << 20;
/// At most this many bytes are kept in total.
const POOL_MAX: usize = 96 << 20;
const POOL_COUNT: usize = 24;

static POOL: Mutex<(Vec<Vec<u8>>, usize)> = Mutex::new((Vec::new(), 0));
/// Takes served from the pool / allocated anew (status line).
pub static REUSED: AtomicUsize = AtomicUsize::new(0);
pub static FRESH: AtomicUsize = AtomicUsize::new(0);
/// Off: always allocate anew (benchmark comparison).
pub static USE_POOL: AtomicBool = AtomicBool::new(true);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A buffer of length `n` with arbitrary (initialized) contents: a kept one
/// when one is large enough, otherwise a new zeroed one.
pub fn take(n: usize) -> Vec<u8> {
    take_kept(n).unwrap_or_else(|| {
        count_fresh(n);
        vec![0u8; n]
    })
}

fn count_fresh(n: usize) {
    if n >= KEEP_MIN {
        FRESH.fetch_add(1, Ordering::Relaxed);
    }
}

/// A kept buffer that holds `n` bytes, as it was handed back (length and
/// contents unchanged). Only one at most half as large again: the result may
/// be kept a long time (recent results count their length, not their capacity).
fn take_any(n: usize) -> Option<Vec<u8>> {
    if n < KEEP_MIN || !USE_POOL.load(Ordering::Relaxed) {
        return None;
    }
    let mut p = lock(&POOL);
    // the smallest kept buffer that holds n bytes
    let i = p.0.iter().enumerate().filter(|(_, v)| (n..=n + n / 2).contains(&v.capacity())).min_by_key(|(_, v)| v.capacity()).map(|(i, _)| i)?;
    let v = p.0.swap_remove(i);
    p.1 -= v.capacity();
    drop(p);
    REUSED.fetch_add(1, Ordering::Relaxed);
    Some(v)
}

/// A kept buffer of length `n` (arbitrary contents), if there is one.
pub fn take_kept(n: usize) -> Option<Vec<u8>> {
    let mut v = take_any(n)?;
    if v.len() >= n {
        v.truncate(n);
    } else {
        v.resize(n, 0); // only the part never written before is zeroed
    }
    Some(v)
}

/// Empty with room for at least `n` bytes (nothing is zeroed).
pub fn take_empty(n: usize) -> Vec<u8> {
    match take_any(n) {
        Some(mut v) => {
            v.clear();
            v
        }
        None => {
            count_fresh(n);
            Vec::with_capacity(n)
        }
    }
}

/// Hand a buffer back for later use. Its contents stay as they are (all
/// written, so `take` can hand out up to `len` bytes without zeroing; the
/// rest is zeroed by `take` when needed, outside the lock).
pub fn give(v: Vec<u8>) {
    let cap = v.capacity();
    if !(KEEP_MIN..=KEEP_MAX).contains(&cap) || !USE_POOL.load(Ordering::Relaxed) {
        return;
    }
    let mut p = lock(&POOL);
    if p.0.len() >= POOL_COUNT || p.1 + cap > POOL_MAX {
        // make room by dropping the smallest kept buffer, if this one is larger
        let Some((i, small)) = p.0.iter().enumerate().min_by_key(|(_, v)| v.capacity()).map(|(i, v)| (i, v.capacity())) else { return };
        if small >= cap || p.1 - small + cap > POOL_MAX {
            return;
        }
        let old = p.0.swap_remove(i);
        p.1 -= old.capacity();
        drop(old);
    }
    p.1 += cap;
    p.0.push(v);
}

/// Hand back a shared buffer when nobody else holds it any more.
pub fn give_arc(v: Arc<Vec<u8>>) {
    if let Ok(v) = Arc::try_unwrap(v) {
        give(v);
    }
}

/// Bytes kept in the pool right now (status line).
pub fn kept() -> usize {
    lock(&POOL).1
}

// ---------------------------------------------------------------- threads

/// Splitting a texture across threads pays only from this much data (RLE:
/// the whole texture, LRLE: its top level), measured on game textures.
pub const SPLIT_MIN_BYTES: usize = 256 << 10;
/// Off: every texture on the calling thread only (benchmark comparison).
pub static SPLIT: AtomicBool = AtomicBool::new(true);
/// Upscales in progress on any game thread.
pub static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// Most upscales seen in progress at once (status line).
pub static ACTIVE_MAX: AtomicUsize = AtomicUsize::new(0);
/// Textures that were split across threads (status line).
pub static SPLIT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Counts an upscale as in progress while alive.
pub struct Busy;
impl Busy {
    pub fn new() -> Self {
        let n = ACTIVE.fetch_add(1, Ordering::Relaxed) + 1;
        ACTIVE_MAX.fetch_max(n, Ordering::Relaxed);
        Busy
    }
}
impl Drop for Busy {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn cores() -> usize {
    static C: OnceLock<usize> = OnceLock::new();
    *C.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Number of threads one texture with this much data is split across: the
/// CPU's share left over by the upscales running on other game threads, at most 4.
pub fn threads_for(bytes: usize) -> usize {
    if bytes < SPLIT_MIN_BYTES || !SPLIT.load(Ordering::Relaxed) {
        return 1;
    }
    let busy = ACTIVE.load(Ordering::Relaxed).max(1);
    (cores() / busy).clamp(1, 4)
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Workers {
    /// (latch of the `run_split` call it belongs to, task)
    queue: Mutex<std::collections::VecDeque<(usize, Task)>>,
    ready: Condvar,
    started: Mutex<usize>,
}

fn workers() -> &'static Workers {
    static W: OnceLock<Workers> = OnceLock::new();
    W.get_or_init(|| Workers { queue: Mutex::new(Default::default()), ready: Condvar::new(), started: Mutex::new(0) })
}

/// Make sure at least `n` worker threads exist. False if none could start.
fn ensure_workers(n: usize) -> bool {
    let w = workers();
    let mut started = lock(&w.started);
    while *started < n {
        let ok = std::thread::Builder::new()
            .name("S4HQxNHQ worker".into())
            .stack_size(256 << 10)
            .spawn(|| {
                let w = workers();
                loop {
                    let task = {
                        let mut q = lock(&w.queue);
                        loop {
                            if let Some((_, t)) = q.pop_front() {
                                break t;
                            }
                            q = w.ready.wait(q).unwrap_or_else(|p| p.into_inner());
                        }
                    };
                    task();
                }
            })
            .is_ok();
        if !ok {
            break;
        }
        *started += 1;
    }
    *started > 0
}

/// Waits for the jobs handed to workers.
struct Latch {
    left: Mutex<usize>,
    done: Condvar,
    ok: AtomicBool,
}

/// Run `jobs`, the first on the calling thread and the others on worker
/// threads, and wait for all of them. All must succeed. Jobs no worker has
/// taken yet (workers busy with another texture) the calling thread runs
/// itself instead of waiting.
pub fn run_split<J: Send>(jobs: Vec<J>, f: impl Fn(J) -> Option<()> + Sync) -> Option<()> {
    if jobs.len() <= 1 || !ensure_workers(jobs.len() - 1) {
        return jobs.into_iter().try_for_each(&f);
    }
    SPLIT_COUNT.fetch_add(1, Ordering::Relaxed);
    let latch = Latch { left: Mutex::new(jobs.len() - 1), done: Condvar::new(), ok: AtomicBool::new(true) };
    let mut it = jobs.into_iter();
    let first = it.next();
    let tag = &latch as *const Latch as usize;
    {
        let (f, latch) = (&f, &latch);
        let mut q = lock(&workers().queue);
        for j in it {
            let task: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(j)));
                if !matches!(r, Ok(Some(()))) {
                    latch.ok.store(false, Ordering::Relaxed);
                }
                let mut left = lock(&latch.left);
                *left -= 1;
                if *left == 0 {
                    latch.done.notify_all();
                }
            });
            // SAFETY: the task borrows `f`, `latch` and the jobs' data, all of
            // which outlive it: this function does not return (and does not
            // unwind, the own job's panic is caught) before every task has
            // counted itself done on the latch.
            let task: Task = unsafe { std::mem::transmute(task) };
            q.push_back((tag, task));
        }
    }
    workers().ready.notify_all();
    let own = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| first.map_or(Some(()), &f)));
    loop {
        let task = {
            let mut q = lock(&workers().queue);
            match q.iter().position(|(t, _)| *t == tag) {
                Some(i) => q.remove(i).map(|(_, t)| t),
                None => None,
            }
        };
        match task {
            Some(t) => t(), // catches its own panic and counts itself done
            None => break,
        }
    }
    let mut left = lock(&latch.left);
    while *left > 0 {
        left = latch.done.wait(left).unwrap_or_else(|p| p.into_inner());
    }
    drop(left);
    match own {
        Ok(r) => r.filter(|_| latch.ok.load(Ordering::Relaxed)),
        Err(p) => std::panic::resume_unwind(p),
    }
}
