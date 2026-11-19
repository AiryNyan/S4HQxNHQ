//! Scan cache (S4HQxNHQ.cache next to the game executable): remembers, per package
//! file, which textures get upscaled and how large the results are, so later
//! starts do not have to read every texture header again. A record is only
//! used while the file's size and modification time are unchanged.

use crate::vfs::Planned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const MAGIC: &[u8; 4] = b"HQXC";
/// Bump whenever the upscalers' output sizes change.
const FORMAT: u32 = 4;

/// Size limit of the cache file. Records of packages that have disappeared
/// are dropped on every save; beyond the limit the least recently used
/// records go first.
pub static MAX_FILE: AtomicUsize = AtomicUsize::new(8 << 20);
/// `last` is only refreshed when it is older than this, so an unchanged
/// setup does not rewrite the file on every start.
const TOUCH_SECS: u64 = 24 * 3600;

pub struct Rec {
    pub size: u64,
    pub mtime: u64,
    pub ents: Vec<Planned>,
    /// When the package was last seen (seconds since 1970).
    pub last: u64,
    /// Seen in this session.
    pub used: bool,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn rec_bytes(key: &str, r: &Rec) -> usize {
    30 + key.len() + 9 * r.ents.len()
}

struct Store {
    path: PathBuf,
    recs: HashMap<String, Rec>,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn parse(d: &[u8]) -> Option<HashMap<String, Rec>> {
    let mut p = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = d.get(p..p.checked_add(n)?)?;
        p += n;
        Some(s)
    };
    if take(4)? != MAGIC || u32::from_le_bytes(take(4)?.try_into().ok()?) != FORMAT {
        return None;
    }
    // Written by another version: its upscalers may size textures differently.
    let vlen = take(1)?[0] as usize;
    if take(vlen)? != crate::VERSION.as_bytes() {
        return None;
    }
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let mut recs = HashMap::new();
    for _ in 0..count {
        let klen = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
        let key = String::from_utf8(take(klen)?.to_vec()).ok()?;
        let size = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let mtime = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let last = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let body = take(n.checked_mul(9)?)?;
        let ents = body.chunks_exact(9).map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]), u32::from_le_bytes([c[4], c[5], c[6], c[7]]), c[8])).collect();
        recs.insert(key, Rec { size, mtime, ents, last, used: false });
    }
    Some(recs)
}

fn store() -> Option<MutexGuard<'static, Store>> {
    Some(STORE.get()?.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Load the cache file. Without this call the cache stays off.
pub fn init(path: PathBuf) {
    let recs = std::fs::read(&path).ok().and_then(|d| parse(&d)).unwrap_or_default();
    let _ = STORE.set(Mutex::new(Store { path, recs }));
}

/// Size and modification time identifying a file's current content.
pub fn stamp(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), t))
}

/// The remembered scan of `key`, if the file is unchanged.
pub fn get(key: &str, stamp: (u64, u64)) -> Option<Vec<Planned>> {
    let mut s = store()?;
    let r = s.recs.get_mut(key)?;
    r.used = true;
    let t = now();
    if t > r.last + TOUCH_SECS {
        r.last = t;
        DIRTY.store(true, Ordering::Relaxed);
    }
    ((r.size, r.mtime) == stamp).then(|| r.ents.clone())
}

pub fn put(key: &str, stamp: (u64, u64), ents: Vec<Planned>) {
    if let Some(mut s) = store() {
        s.recs.insert(key.to_string(), Rec { size: stamp.0, mtime: stamp.1, ents, last: now(), used: true });
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Correct one texture's size (`size` > 0) or forget the package (`size` == 0).
pub fn amend(key: &str, ord: u32, size: u32) {
    if let Some(mut s) = store() {
        if size == 0 {
            s.recs.remove(key);
        } else if let Some(e) = s.recs.get_mut(key).and_then(|r| r.ents.iter_mut().find(|e| e.0 == ord)) {
            e.1 = size;
        }
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Write the cache file if anything changed since the last save.
pub fn save_if_dirty() {
    if !DIRTY.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(mut s) = store() else { return };
    // Forget packages that no longer exist (removed mods).
    s.recs.retain(|k, r| k.len() <= u16::MAX as usize && (r.used || Path::new(k).exists()));
    // Over the limit: drop the least recently used records until it fits.
    let max = MAX_FILE.load(Ordering::Relaxed);
    let mut total: usize = 13 + crate::VERSION.len() + s.recs.iter().map(|(k, r)| rec_bytes(k, r)).sum::<usize>();
    if total > max {
        let mut order: Vec<(u64, String)> = s.recs.iter().map(|(k, r)| (r.last, k.clone())).collect();
        order.sort();
        for (_, k) in order {
            if total <= max {
                break;
            }
            if let Some(r) = s.recs.remove(&k) {
                total -= rec_bytes(&k, &r);
            }
        }
    }
    let mut d = Vec::new();
    d.extend_from_slice(MAGIC);
    d.extend_from_slice(&FORMAT.to_le_bytes());
    d.push(crate::VERSION.len() as u8);
    d.extend_from_slice(crate::VERSION.as_bytes());
    d.extend_from_slice(&(s.recs.len() as u32).to_le_bytes());
    for (k, r) in &s.recs {
        d.extend_from_slice(&(k.len() as u16).to_le_bytes());
        d.extend_from_slice(k.as_bytes());
        d.extend_from_slice(&r.size.to_le_bytes());
        d.extend_from_slice(&r.mtime.to_le_bytes());
        d.extend_from_slice(&r.last.to_le_bytes());
        d.extend_from_slice(&(r.ents.len() as u32).to_le_bytes());
        for e in &r.ents {
            d.extend_from_slice(&e.0.to_le_bytes());
            d.extend_from_slice(&e.1.to_le_bytes());
            d.push(e.2);
        }
    }
    let tmp = s.path.with_extension("cache.tmp");
    // rename replaces the old file in one step, so a crash never leaves no cache
    if std::fs::write(&tmp, &d).is_ok() {
        let _ = std::fs::rename(&tmp, &s.path);
    }
}
