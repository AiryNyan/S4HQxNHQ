//! S4HQxNHQ: loaded as d3d11.dll (proxy) by TS4_x64.exe. Lets standard-resolution
//! sim textures be used under the HQ (4096) sim texture setting by upscaling
//! them in memory as the game reads them from .package files.
//!
//! Works purely at API boundaries: the proxy forwards D3D11, and the game's
//! import-table pointers for a few kernel32 file functions are redirected.
//! No game code is read or modified, and no file on disk is changed.
#![allow(non_snake_case, clippy::missing_safety_doc, function_casts_as_integer)]

mod bytes;
mod cache;
mod frames;
mod inflate;
mod lrle;
mod pool;
mod rle;
mod sgr;
mod vfs;

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use vfs::{Cont, Pkg};

type P = *mut c_void;
type HRESULT = i32;

#[repr(C)]
struct Mbi {
    base: P,
    alloc_base: P,
    alloc_protect: u32,
    partition: u32,
    size: usize,
    state: u32,
    protect: u32,
    ty: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> P;
    fn GetModuleHandleW(name: *const u16) -> P;
    fn GetProcAddress(m: P, name: *const u8) -> P;
    fn VirtualProtect(addr: P, size: usize, newp: u32, oldp: *mut u32) -> i32;
    fn VirtualQuery(addr: P, mbi: *mut Mbi, len: usize) -> usize;
    fn GetModuleFileNameW(m: P, buf: *mut u16, n: u32) -> u32;
    fn GetSystemDirectoryW(buf: *mut u16, n: u32) -> u32;
    fn DisableThreadLibraryCalls(m: P) -> i32;
    fn GetFileType(h: P) -> u32;
    fn GetFinalPathNameByHandleW(h: P, buf: *mut u16, n: u32, flags: u32) -> u32;
    fn SetFilePointerEx(h: P, dist: i64, newpos: *mut i64, method: u32) -> i32;
    fn Sleep(ms: u32);
    fn AddVectoredExceptionHandler(first: u32, h: unsafe extern "system" fn(*const [*const u8; 2]) -> i32) -> P;
    fn GetModuleHandleExW(flags: u32, addr: *const u16, m: *mut P) -> i32;
    fn GetCurrentProcess() -> P;
    fn K32GetProcessMemoryInfo(process: P, counters: *mut [usize; 9], size: u32) -> i32;
    fn ReadProcessMemory(process: P, base: P, buf: P, size: usize, read: *mut usize) -> i32;
    fn GetFileInformationByHandle(h: P, info: *mut [u32; 13]) -> i32;
}

const PAGE_READWRITE: u32 = 0x04;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
/// Bytes of game memory copied per ReadProcessMemory call when looking for import slots.
const SCAN_CHUNK: usize = 64 << 10;

// ---------------------------------------------------------------- logging

/// S4HQxNHQ.log is capped: when it passes LOG_MAX the oldest part is dropped and
/// only the newest LOG_KEEP bytes stay.
const LOG_MAX: u64 = 1 << 20;
const LOG_KEEP: usize = 512 << 10;

struct Log {
    path: std::path::PathBuf,
    file: Option<File>,
    size: u64,
}

impl Log {
    fn open(&mut self) {
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
    }
    fn trim(&mut self) {
        self.file = None;
        if let Ok(d) = std::fs::read(&self.path) {
            let from = d.len().saturating_sub(LOG_KEEP);
            // start at a line boundary
            let from = d[from..].iter().position(|b| *b == b'\n').map_or(d.len(), |i| from + i + 1);
            let _ = std::fs::write(&self.path, &d[from..]);
        }
        self.open();
    }
    fn line(&mut self, s: &str) {
        if self.size + s.len() as u64 + 2 > LOG_MAX {
            self.trim();
        }
        if let Some(f) = self.file.as_mut() {
            if writeln!(f, "{s}").is_ok() {
                self.size += s.len() as u64 + 1;
            }
            let _ = f.flush();
        }
    }
}

fn exe_sibling(name: &str) -> std::path::PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), 1024) } as usize;
    std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n])).with_file_name(name)
}

fn log_file() -> &'static Mutex<Log> {
    static L: OnceLock<Mutex<Log>> = OnceLock::new();
    L.get_or_init(|| {
        let mut l = Log { path: exe_sibling("S4HQxNHQ.log"), file: None, size: 0 };
        l.open();
        Mutex::new(l)
    })
}

/// A file named S4HQxNHQ.debug next to the game executable: troubleshooting mode
/// (detailed logging, crash safety levels, scan cache off).
fn debug_file() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| exe_sibling("S4HQxNHQ.debug").exists())
}

/// Detailed logging: always in the log-collection build, otherwise only in
/// troubleshooting mode. Everything else behaves like the normal build.
fn verbose() -> bool {
    cfg!(feature = "diag") || debug_file()
}

/// Working set and committed memory of the game process, in MB.
fn process_memory() -> (usize, usize) {
    let mut c = [0usize; 9];
    c[0] = std::mem::size_of::<[usize; 9]>(); // cb (low 32 bits); the page fault count shares this slot
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c[0] as u32) } == 0 {
        return (0, 0);
    }
    (c[2] >> 20, c[7] >> 20)
}

pub(crate) fn note(s: &str) {
    log_file().lock().unwrap_or_else(|e| e.into_inner()).line(s);
}

/// Logged only in debug mode.
fn note_v(s: impl FnOnce() -> String) {
    if verbose() {
        note(&s());
    }
}

/// Like `note` but never blocks (used from the exception handler).
fn note_try(s: &str) {
    if let Ok(mut g) = log_file().try_lock() {
        g.line(s);
    }
}

/// Fixed-size text buffer: formatting into it never allocates, so it is
/// safe inside the exception handler even when the heap is damaged.
struct StackBuf {
    b: [u8; 512],
    n: usize,
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// Write one line without allocating and without waiting for the log lock.
fn note_raw(s: &str) {
    if let Ok(mut g) = log_file().try_lock() {
        if let Some(f) = g.file.as_mut() {
            let _ = f.write_all(s.as_bytes());
            let _ = f.write_all(b"\n");
            let _ = f.flush();
            g.size += s.len() as u64 + 1;
        }
    }
}

// ------------------------------------------------------- crash diagnostics

static OUR_BASE: AtomicUsize = AtomicUsize::new(0);
static OUR_SIZE: AtomicUsize = AtomicUsize::new(0);
/// Exceptions recorded, inside S4HQxNHQ and elsewhere, counted apart so that
/// exceptions the game handles itself cannot use up the record of our own.
static N_EXC_IN: AtomicU64 = AtomicU64::new(0);
static N_EXC_OUT: AtomicU64 = AtomicU64::new(0);

/// Records fatal-looking exceptions, then lets normal handling continue
/// (changes nothing). The normal build records only faults inside
/// S4HQxNHQ's own code; detailed logging records every one, with the
/// module it happened in and S4HQxNHQ return addresses found on the stack.
/// Never allocates: it may run while the heap is damaged.
unsafe extern "system" fn veh(ep: *const [*const u8; 2]) -> i32 {
    use std::fmt::Write as _;
    let (rec, ctx) = ((*ep)[0], (*ep)[1]);
    let code = *(rec as *const u32);
    if !matches!(code, 0xC0000005 | 0xC000001D | 0xC0000094 | 0xC0000096 | 0xC0000409 | 0xC0000374 | 0xC00000FD) {
        return 0;
    }
    let addr = *(rec.add(16) as *const usize);
    let (base, size) = (OUR_BASE.load(Ordering::Relaxed), OUR_SIZE.load(Ordering::Relaxed));
    let inside = addr >= base && addr < base + size;
    if !inside && !verbose() {
        return 0;
    }
    if (if inside { &N_EXC_IN } else { &N_EXC_OUT }).fetch_add(1, Ordering::Relaxed) >= 6 {
        return 0;
    }
    let mut out = StackBuf { b: [0; 512], n: 0 };
    if inside {
        let _ = write!(out, "CRASH inside S4HQxNHQ {VERSION}: code {code:#x} at S4HQxNHQ+{:#x}", addr - base);
    } else {
        // module name, from a fixed buffer
        let mut m: P = std::ptr::null_mut();
        let mut wbuf = [0u16; 260];
        let mut n = 0usize;
        if GetModuleHandleExW(6, addr as *const u16, &mut m) != 0 && !m.is_null() {
            n = GetModuleFileNameW(m, wbuf.as_mut_ptr(), 260) as usize;
        }
        let start = wbuf[..n].iter().rposition(|c| *c == b'\\' as u16 || *c == b'/' as u16).map_or(0, |i| i + 1);
        let _ = write!(out, "EXCEPTION {code:#x} at ");
        for c in char::decode_utf16(wbuf[start..n].iter().copied()) {
            let _ = out.write_char(c.unwrap_or('?'));
        }
        let _ = write!(out, "+{:#x}", addr.wrapping_sub(m as usize));
    }
    if code == 0xC0000005 {
        let (i0, i1) = (*(rec.add(32) as *const usize), *(rec.add(40) as *const usize));
        let _ = write!(out, " ({} address {i1:#x})", if i0 == 1 { "writing" } else if i0 == 8 { "executing" } else { "reading" });
    }
    if verbose() {
        // S4HQxNHQ return addresses on the stack (is our code involved?)
        let rsp = *(ctx.add(0x98) as *const usize);
        let mut mbi: Mbi = std::mem::zeroed();
        let _ = write!(out, "; S4HQxNHQ on stack:");
        let mut found = 0;
        if rsp != 0 && VirtualQuery(rsp as P, &mut mbi, std::mem::size_of::<Mbi>()) != 0 && mbi.state == 0x1000 {
            let end = (mbi.base as usize + mbi.size).min(rsp + 8 * 2048);
            let mut p = rsp & !7;
            while p + 8 <= end && found < 8 {
                let v = *(p as *const usize);
                if v >= base && v < base + size {
                    let _ = write!(out, " {:#x}", v - base);
                    found += 1;
                }
                p += 8;
            }
        }
        if found == 0 {
            let _ = write!(out, " none");
        }
    }
    let _ = write!(out, "; upscaled reads so far {}", N_VREADS.load(Ordering::Relaxed));
    if let Ok(s) = std::str::from_utf8(&out.b[..out.n]) {
        note_raw(s);
    }
    0
}

// ------------------------------------------------------------ safety level

const VERSION: &str = "1.1.1";

fn level_path() -> std::path::PathBuf {
    exe_sibling("S4HQxNHQ.level")
}

fn write_level(state: u8) {
    if !debug_file() {
        return;
    }
    let l = vfs::LEVEL.load(Ordering::Relaxed);
    let _ = std::fs::write(level_path(), format!("{VERSION} {l} {state}"));
}

/// A run that neither got sims on screen nor exited cleanly counts as a
/// crash; the next run then switches one risky feature off.
fn init_level() {
    if !debug_file() {
        return; // normal use: everything on, no state file
    }
    let mut level = 0u8;
    let mut why = "";
    if let Ok(t) = std::fs::read_to_string(level_path()) {
        let v: Vec<&str> = t.split_whitespace().collect();
        if v.len() == 3 && v[0] == VERSION {
            level = v[1].parse().unwrap_or(0);
            if v[2] == "1" && level < 2 {
                level += 1;
                why = " (previous run did not get sims on screen or exit cleanly)";
            }
        }
    }
    vfs::LEVEL.store(level.min(2), Ordering::Relaxed);
    write_level(1);
    note(&format!("safety level {level}{why}: dense packages {}, LRLE {}", if level < 1 { "on" } else { "OFF" }, if level < 2 { "on" } else { "OFF" }));
}

// ---------------------------------------------------------------- file IO

static O_READFILE: AtomicUsize = AtomicUsize::new(0);
static O_CLOSEHANDLE: AtomicUsize = AtomicUsize::new(0);
static O_SFPEX: AtomicUsize = AtomicUsize::new(0);
static O_SFP: AtomicUsize = AtomicUsize::new(0);
static O_GFSEX: AtomicUsize = AtomicUsize::new(0);
static O_GFS: AtomicUsize = AtomicUsize::new(0);
static O_CFW: AtomicUsize = AtomicUsize::new(0);
static O_CFA: AtomicUsize = AtomicUsize::new(0);

static N_VREADS: AtomicU64 = AtomicU64::new(0);
static N_VBYTES: AtomicU64 = AtomicU64::new(0);
static N_PKG_SCANNED: AtomicU64 = AtomicU64::new(0);
static N_PKG_CACHED: AtomicU64 = AtomicU64::new(0);
static N_TEX: AtomicU64 = AtomicU64::new(0);
static SCAN_MS: AtomicU64 = AtomicU64::new(0);
/// Upscaled reads by kind (see `vfs::VEntry::kind`): RLE2, RLES, LRLE, -, specular served empty, L8.
static N_KIND: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static N_DIAG: AtomicU64 = AtomicU64::new(0);
/// Time spent producing upscaled data (read + inflate + upscale), microseconds, and the slowest single read.
static UP_US: AtomicU64 = AtomicU64::new(0);
static UP_MAX_US: AtomicU64 = AtomicU64::new(0);
/// Package and read position of the slowest virtual read (status line).
static UP_MAX_AT: Mutex<String> = Mutex::new(String::new());

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rd<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn wr<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Run hook-side logic so that a bug in it cannot take the game down: a
/// panic is logged by the panic hook and the caller falls back to the
/// untouched behaviour.
fn guarded<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Called by the upscaler when a cached size turned out wrong (see cache::amend).
pub(crate) fn report(key: &str, ord: u32, size: u32) {
    cache::amend(key, ord, size);
}

/// Sim-texture reads that were passed through unchanged, by description (debug mode).
fn diag() -> &'static Mutex<std::collections::BTreeMap<String, u64>> {
    static S: OnceLock<Mutex<std::collections::BTreeMap<String, u64>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

macro_rules! orig {
    ($a:expr, $t:ty) => {
        std::mem::transmute::<usize, $t>($a.load(Ordering::SeqCst))
    };
}

type PkgCell = Arc<OnceLock<Option<Arc<Pkg>>>>;

/// Packages by lowercase path. The cell is filled by the first thread that
/// needs the package; the map lock is not held while scanning.
fn pkgs() -> &'static Mutex<HashMap<String, PkgCell>> {
    static S: OnceLock<Mutex<HashMap<String, PkgCell>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A package handle the game reads from.
struct HState {
    pkg: Arc<Pkg>,
    cont: Mutex<Cont>,
}

/// Identity of an open disk file: (volume serial number, file index).
type FileId = (u32, u64);

/// A handle the game has used: the file it referred to and, for a
/// virtualized package, its state (None = left as it is).
type Known = (Option<FileId>, Option<Arc<HState>>);

/// Open handles the game has used.
fn handles() -> &'static RwLock<HashMap<usize, Known>> {
    static S: OnceLock<RwLock<HashMap<usize, Known>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(HashMap::new()))
}

fn file_id(h: P) -> Option<FileId> {
    let mut b = [0u32; 13];
    unsafe {
        if GetFileType(h) != 1 || GetFileInformationByHandle(h, &mut b) == 0 {
            return None;
        }
    }
    Some((b[7], ((b[11] as u64) << 32) | b[12] as u64))
}

/// Handles opened or closed outside the game executable's own imports are
/// not seen, and their values can come back for another file. At points
/// where a file is typically (re)examined (size queries, seeks to the start
/// or end, reading a package from its start) the remembered file is checked
/// against the handle's actual file and forgotten when it differs.
/// A handle whose identity cannot be read now (not a disk file, or the query
/// failed) is left as it is: only a positively different file counts.
fn recheck(h: P) {
    let Some(stored) = rd(handles()).get(&(h as usize)).map(|e| e.0) else { return };
    let now = file_id(h);
    if now.is_some() && now != stored {
        forget(h);
    }
}

/// Open handles of GraphicsRules.sgr, with the edited copy of the file.
fn sgrs() -> &'static RwLock<HashMap<usize, Arc<Vec<u8>>>> {
    static S: OnceLock<RwLock<HashMap<usize, Arc<Vec<u8>>>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 0 = no GraphicsRules.sgr read seen yet, 1 = HQ setting in effect.
static HQ_SEEN: AtomicBool = AtomicBool::new(false);

/// The game opened a GraphicsRules.sgr: prepare the HQ version of it.
fn sgr_opened(h: P, path: &str) {
    let Ok(src) = std::fs::read(path) else { return };
    let (patched, changed, already) = sgr::patch(&src);
    if changed + already == 0 {
        note(&format!("{path}: no sim texture settings found, left as it is"));
        return;
    }
    HQ_SEEN.store(true, Ordering::Relaxed);
    if changed > 0 {
        wr(sgrs()).insert(h as usize, Arc::new(patched));
    }
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) || verbose() {
        note(&format!("HQ sim texture size (4096) applied in memory to {path}: {changed} lines changed, {already} already set; the file itself is untouched"));
    }
}

/// What is known about a handle, without looking the file up.
fn known(h: P) -> Option<Option<Arc<HState>>> {
    rd(handles()).get(&(h as usize)).map(|e| e.1.clone())
}

/// Drop what is known about a handle value (it was closed or is being reused).
fn forget(h: P) {
    if known(h).is_some() {
        wr(handles()).remove(&(h as usize));
        wr(sgrs()).remove(&(h as usize));
    }
}

/// The game's own caches and databases (in its user-data folder) are never
/// virtualized. Anything under a Mods folder is content, whatever its name.
fn excluded(lower_path: &str, name: &str) -> bool {
    !lower_path.contains("\\mods\\") && ["local", "avatarcache", "clientdb", "accountdatadb", "housedescription", "spotlight"].iter().any(|p| name.starts_with(p))
}

/// `Pkg::build`, tried a few times when the file cannot be read (it may be
/// held by a virus scanner or a mod manager for a moment).
fn build_retry(p: &std::path::Path, lower: &str, cached: Option<&[vfs::Planned]>) -> Result<Option<Pkg>, String> {
    let mut r = Pkg::build(p, lower, cached, verbose());
    for _ in 0..3 {
        if r.is_ok() {
            break;
        }
        unsafe { Sleep(50) };
        r = Pkg::build(p, lower, cached, verbose());
    }
    r
}

/// Scan a package. Err: it could not be read; nothing is remembered about it,
/// so it is tried again when the game opens it next (or next start).
fn scan(path: &str, lower: &str) -> Result<Option<Arc<Pkg>>, String> {
    if TEST_PANIC_SCAN.load(Ordering::Relaxed) {
        panic!("test panic inside a package scan");
    }
    let t0 = std::time::Instant::now();
    let p = std::path::Path::new(path);
    let stamp = cache::stamp(p);
    let cached = stamp.and_then(|s| cache::get(lower, s));
    let mut pkg = None;
    let mut from_cache = false;
    if let Some(list) = &cached {
        from_cache = true;
        if !list.is_empty() || verbose() {
            pkg = build_retry(p, lower, Some(list))?;
            if !list.is_empty() {
                from_cache = pkg.is_some(); // a record that no longer fits is rebuilt
            }
        }
    }
    if !from_cache {
        pkg = build_retry(p, lower, None)?;
        // Only a complete scan is remembered: a read failure never becomes
        // "nothing to upscale" in the cache.
        if let Some(s) = stamp {
            cache::put(lower, s, pkg.as_ref().map(|p| p.plan.clone()).unwrap_or_default());
        }
    }
    let ms = t0.elapsed().as_millis() as u64;
    SCAN_MS.fetch_add(ms, Ordering::Relaxed);
    if from_cache { &N_PKG_CACHED } else { &N_PKG_SCANNED }.fetch_add(1, Ordering::Relaxed);
    if let Some(p) = pkg.as_ref().filter(|p| !p.ents.is_empty() || p.skipped > 0) {
        N_TEX.fetch_add(p.ents.len() as u64, Ordering::Relaxed);
        if p.skipped > 0 {
            note(&format!("{}: {} textures cannot be upscaled (package too large)", p.name, p.skipped));
        }
        note_v(|| {
            let parent = path.rsplit(['\\', '/']).nth(1).unwrap_or("");
            let k = |n: u8| p.ents.iter().filter(|e| e.kind == n).count();
            format!(
                "package {parent}/{}: {} textures will be upscaled (RLE2 {}, RLES {}, LRLE {}, L8 {}; {} skipped{}{}; scan {} ms)",
                p.name,
                p.ents.len() - k(4),
                k(0),
                k(1),
                k(2),
                k(5),
                p.skipped,
                if p.dense { "; dense" } else { "" },
                if k(4) > 0 { format!("; specular off {} (no shadow map)", k(4)) } else { String::new() },
                ms
            )
        });
    }
    Ok(pkg.map(Arc::new))
}

unsafe fn resolve(h: P) -> Option<Arc<HState>> {
    if let Some(v) = known(h) {
        return v;
    }
    let mut found: Option<Arc<HState>> = None;
    let id = file_id(h);
    if GetFileType(h) == 1 {
        let mut buf = [0u16; 2048];
        let n = GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), 2048, 0) as usize;
        if n > 0 && n < 2048 {
            let path = String::from_utf16_lossy(&buf[..n]);
            let lower = path.to_lowercase();
            let name = lower.rsplit(['\\', '/']).next().unwrap_or("");
            if name == "graphicsrules.sgr" {
                sgr_opened(h, &path);
            } else if lower.ends_with(".package") && !excluded(&lower, name) {
                let cell = lk(pkgs()).entry(lower.clone()).or_default().clone();
                let mut retry = false;
                let scanned = cell.get_or_init(|| match guarded(|| scan(&path, &lower)) {
                    Some(Ok(p)) => p,
                    r => {
                        static N: AtomicU64 = AtomicU64::new(0);
                        if N.fetch_add(1, Ordering::Relaxed) < 20 {
                            let why = match r {
                                Some(Err(e)) => e,
                                _ => "internal error".into(),
                            };
                            note(&format!("{}: could not be read ({why}), left as it is for now", path.rsplit(['\\', '/']).next().unwrap_or("")));
                        }
                        retry = true;
                        None
                    }
                });
                if retry {
                    // Not remembered: the next handle to this file scans again,
                    // up to a few times per session (each try may wait a little).
                    static FAILS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
                    let mut f = lk(FAILS.get_or_init(Default::default));
                    let n = f.entry(lower.clone()).or_insert(0);
                    *n += 1;
                    if *n < 3 {
                        lk(pkgs()).remove(&lower);
                    }
                }
                if let Some(pkg) = scanned {
                    found = Some(Arc::new(HState { pkg: pkg.clone(), cont: Mutex::new(None) }));
                }
            }
        }
    }
    wr(handles()).insert(h as usize, (id, found.clone()));
    found
}

unsafe extern "system" fn h_readfile(h: P, buf: P, n: u32, read: *mut u32, ovl: *const [usize; 4]) -> i32 {
    let f = orig!(O_READFILE, unsafe extern "system" fn(P, P, u32, *mut u32, *const [usize; 4]) -> i32);
    if !ovl.is_null() || buf.is_null() {
        return f(h, buf, n, read, ovl);
    }
    let Some(Some(st)) = guarded(|| resolve(h)) else {
        // Not a package. The settings file gets its HQ values laid over.
        let hq = rd(sgrs()).get(&(h as usize)).cloned();
        let Some(hq) = hq else { return f(h, buf, n, read, ovl) };
        let mut pos = -1i64;
        if SetFilePointerEx(h, 0, &mut pos, 1) == 0 || pos < 0 {
            return f(h, buf, n, read, ovl);
        }
        let mut got = 0u32;
        let r = f(h, buf, n, &mut got, ovl);
        if r != 0 && got > 0 {
            guarded(|| {
                let data = std::slice::from_raw_parts_mut(buf as *mut u8, got as usize);
                let pos = pos as usize;
                if pos < hq.len() {
                    let k = data.len().min(hq.len() - pos);
                    data[..k].copy_from_slice(&hq[pos..pos + k]);
                }
            });
        }
        if !read.is_null() {
            *read = got;
        }
        return r;
    };
    let mut pos = -1i64;
    if SetFilePointerEx(h, 0, &mut pos, 1) == 0 || pos < 0 {
        return f(h, buf, n, read, ovl);
    }
    let pos = pos as u64;
    if pos == 0 {
        // Reading a package from its start: make sure the handle still is that package.
        guarded(|| recheck(h));
        if known(h).is_none() {
            return h_readfile(h, buf, n, read, ovl);
        }
    }
    let pkg = &st.pkg;
    if pos >= pkg.eof && !pkg.ents.is_empty() {
        let served = guarded(|| {
            let out = std::slice::from_raw_parts_mut(buf as *mut u8, n as usize);
            if N_VREADS.load(Ordering::Relaxed) < 3 {
                note_v(|| format!("serving virtual read: {} pos={pos} len={n} dense={}", pkg.name, pkg.dense));
            }
            if TEST_PANIC.swap(false, Ordering::Relaxed) {
                panic!("test panic inside a virtual read");
            }
            if TEST_FAULT.load(Ordering::Relaxed) {
                std::ptr::read_volatile(0x10 as *const u32);
            }
            let mut cont: Cont = lk(&st.cont).take();
            let t0 = std::time::Instant::now();
            let r = pkg.read_virtual(pos, out, &mut cont);
            let us = t0.elapsed().as_micros() as u64;
            UP_US.fetch_add(us, Ordering::Relaxed);
            frames::work(us);
            if UP_MAX_US.fetch_max(us, Ordering::Relaxed) < us {
                *lk(&UP_MAX_AT) = format!("{} pos={pos} len={n}", pkg.name);
            }
            *lk(&st.cont) = cont;
            r
        });
        // After an internal error the read reports end of file.
        let (got, kind) = served.unwrap_or((0, 3));
        if let Some(k) = N_KIND.get(kind as usize) {
            k.fetch_add(1, Ordering::Relaxed);
        }
        SetFilePointerEx(h, got as i64, std::ptr::null_mut(), 1);
        if !read.is_null() {
            *read = got as u32;
        }
        let c = N_VREADS.fetch_add(1, Ordering::Relaxed) + 1;
        N_VBYTES.fetch_add(got as u64, Ordering::Relaxed);
        if c == 1 || c == 10 || c == 100 || c == 1000 || c == 10000 || c == 100000 {
            note_v(|| format!("upscaled texture reads: {c} ({} MB served); last: {} pos={pos} len={n} got={got}", N_VBYTES.load(Ordering::Relaxed) >> 20, pkg.name));
        }
        return 1;
    }
    let mut got = 0u32;
    let r = f(h, buf, n, &mut got, ovl);
    if r != 0 && got > 0 {
        guarded(|| {
            let data = std::slice::from_raw_parts_mut(buf as *mut u8, got as usize);
            pkg.overlay(pos, data);
            if pkg.skipped == 0 && pkg.bypassed(pos) && !pkg.warned.swap(true, Ordering::Relaxed) {
                note(&format!(
                    "WARNING: {} was read before S4HQxNHQ was ready, so its standard textures are not upscaled this session (sims wearing them may look black). Restarting the game usually fixes this.",
                    pkg.name
                ));
            }
            if !pkg.watch.is_empty() && vfs::LEVEL.load(Ordering::Relaxed) < 2 {
                if let Some((ty, comp)) = pkg.watched(pos) {
                    let d = vfs::describe(ty, data, comp);
                    if d != "DDS other" {
                        *lk(diag()).entry(d).or_insert(0) += 1;
                        N_DIAG.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    if !read.is_null() {
        *read = got;
    }
    r
}

unsafe extern "system" fn h_closehandle(h: P) -> i32 {
    let f = orig!(O_CLOSEHANDLE, unsafe extern "system" fn(P) -> i32);
    guarded(|| forget(h));
    f(h)
}

type CreateFileFn = unsafe extern "system" fn(P, u32, u32, P, u32, u32, P) -> P;

// A handle value handed out by CreateFile is new, whatever an earlier owner
// of the same value was (it may have been closed without us seeing it).
unsafe extern "system" fn h_createfilew(name: P, access: u32, share: u32, sa: P, disp: u32, flags: u32, tmpl: P) -> P {
    let r = orig!(O_CFW, CreateFileFn)(name, access, share, sa, disp, flags, tmpl);
    guarded(|| forget(r));
    r
}

unsafe extern "system" fn h_createfilea(name: P, access: u32, share: u32, sa: P, disp: u32, flags: u32, tmpl: P) -> P {
    let r = orig!(O_CFA, CreateFileFn)(name, access, share, sa, disp, flags, tmpl);
    guarded(|| forget(r));
    r
}

/// An explicit seek ends a multi-call read of a virtual entry. A zero move
/// from the current position only asks where the file pointer is, so it must
/// not: in a dense package the next read could otherwise be taken for the
/// start of another entry.
/// A seek to exactly where the read left off (absolute or relative) also
/// keeps it going, so a reader that positions itself before every chunk is
/// served the same entry.
fn seek_seen(h: P, method: u32, dist: i64) {
    if method == 1 && dist == 0 {
        return;
    }
    if (method == 0 && dist == 0) || method == 2 {
        recheck(h);
    }
    if let Some(Some(st)) = known(h) {
        let mut c = lk(&st.cont);
        let Some((_, next, _)) = *c else { return };
        let target = match method {
            0 => Some(dist),
            1 => {
                let mut cur = 0i64;
                (unsafe { SetFilePointerEx(h, 0, &mut cur, 1) } != 0).then(|| cur + dist)
            }
            _ => None,
        };
        if target != Some(next as i64) {
            *c = None;
        }
    }
}

unsafe extern "system" fn h_sfpex(h: P, dist: i64, newpos: *mut i64, method: u32) -> i32 {
    let f = orig!(O_SFPEX, unsafe extern "system" fn(P, i64, *mut i64, u32) -> i32);
    guarded(|| seek_seen(h, method, dist));
    if method == 2 {
        if let Some(Some(st)) = guarded(|| resolve(h)) {
            // FILE_END must be relative to the size we report.
            return f(h, st.pkg.vend as i64 + dist, newpos, 0);
        }
    }
    f(h, dist, newpos, method)
}

unsafe extern "system" fn h_sfp(h: P, lo: i32, hi: *mut i32, method: u32) -> u32 {
    let f = orig!(O_SFP, unsafe extern "system" fn(P, i32, *mut i32, u32) -> u32);
    let dist = if hi.is_null() { lo as i64 } else { ((*hi as i64) << 32) | (lo as u32 as i64) };
    guarded(|| seek_seen(h, method, dist));
    if method == 2 {
        if let Some(Some(st)) = guarded(|| resolve(h)) {
            if st.pkg.vend != st.pkg.eof {
                let new = st.pkg.vend as i64 + dist;
                let mut h2 = (new >> 32) as i32;
                let r = f(h, new as i32, &mut h2, 0);
                if !hi.is_null() {
                    *hi = h2;
                }
                return r;
            }
        }
    }
    f(h, lo, hi, method)
}

unsafe extern "system" fn h_gfsex(h: P, out: *mut i64) -> i32 {
    let f = orig!(O_GFSEX, unsafe extern "system" fn(P, *mut i64) -> i32);
    let r = f(h, out);
    if r != 0 && !out.is_null() {
        guarded(|| recheck(h));
        if let Some(Some(st)) = guarded(|| resolve(h)) {
            *out = st.pkg.vend as i64;
        }
    }
    r
}

unsafe extern "system" fn h_gfs(h: P, hi: *mut u32) -> u32 {
    let f = orig!(O_GFS, unsafe extern "system" fn(P, *mut u32) -> u32);
    let r = f(h, hi);
    if r != u32::MAX {
        guarded(|| recheck(h));
        if let Some(Some(st)) = guarded(|| resolve(h)) {
            if !hi.is_null() {
                *hi = (st.pkg.vend >> 32) as u32;
            }
            return st.pkg.vend as u32;
        }
    }
    r
}

/// Visit every 8-byte slot equal to `from` in the main module's data sections;
/// replace it with `to` when `write` is set, otherwise only count (and stop
/// as soon as every `from` has been seen once).
unsafe fn swap_imports(pairs: &[(usize, usize)], write: bool) -> Vec<usize> {
    let mut hits = vec![0usize; pairs.len()];
    let base = GetModuleHandleW(std::ptr::null()) as usize;
    if base == 0 {
        return hits;
    }
    let pe = base + *((base + 0x3c) as *const u32) as usize;
    let nsec = *((pe + 6) as *const u16) as usize;
    let optsz = *((pe + 20) as *const u16) as usize;
    let sec0 = pe + 24 + optsz;
    let mut chunk = vec![0usize; SCAN_CHUNK / 8];
    'sections: for i in 0..nsec {
        let sh = sec0 + i * 40;
        let vsize = *((sh + 8) as *const u32) as usize;
        let va = *((sh + 12) as *const u32) as usize;
        let chars = *((sh + 36) as *const u32);
        if chars & 0x2000_0000 != 0 {
            continue; // executable section: never touched
        }
        let (mut a, end) = (base + va, base + va + vsize);
        while a < end {
            let mut mbi: Mbi = std::mem::zeroed();
            if VirtualQuery(a as P, &mut mbi, std::mem::size_of::<Mbi>()) == 0 {
                break;
            }
            let rend = (mbi.base as usize + mbi.size).min(end);
            let readable = mbi.state == 0x1000 && mbi.protect & 0x100 == 0 && mbi.protect & 0xEE != 0;
            if readable {
                // The DRM loader may change page protection at any moment, so
                // memory is copied with ReadProcessMemory (fails instead of
                // faulting) and scanned in the copy.
                let mut p = (a + 7) & !7;
                while p + 8 <= rend {
                    let len = (rend - p).min(SCAN_CHUNK) & !7;
                    let mut got = 0usize;
                    if ReadProcessMemory(GetCurrentProcess(), p as P, chunk.as_mut_ptr() as P, len, &mut got) == 0 || got != len {
                        p += len;
                        continue;
                    }
                    for (w, v) in chunk[..len / 8].iter().enumerate() {
                        for (k, (from, to)) in pairs.iter().enumerate() {
                            if *v != *from || *from == 0 {
                                continue;
                            }
                            if !write {
                                hits[k] += 1;
                                continue;
                            }
                            let slot = p + w * 8;
                            // Keep execute rights if the page has them: another
                            // thread may be running code on it.
                            let prot = if mbi.protect & 0xF0 != 0 { PAGE_EXECUTE_READWRITE } else { PAGE_READWRITE };
                            let mut old = 0u32;
                            if VirtualProtect(slot as P, 8, prot, &mut old) != 0 {
                                let s = &*(slot as *const AtomicUsize);
                                if s.compare_exchange(*from, *to, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                                    hits[k] += 1;
                                }
                                VirtualProtect(slot as P, 8, old, &mut old);
                            }
                        }
                    }
                    p += len;
                    if !write && hits.iter().all(|n| *n > 0) {
                        break 'sections;
                    }
                }
            }
            a = rend.max(a + 0x1000);
        }
    }
    hits
}

static CONFIRMED: AtomicBool = AtomicBool::new(false);
/// Test harness only (HQX_FORCE + HQX_PANIC / HQX_FAULT / HQX_PANIC_SCAN):
/// fail one virtual read, crash inside our code, or fail a package scan on purpose.
static TEST_PANIC: AtomicBool = AtomicBool::new(false);
static TEST_FAULT: AtomicBool = AtomicBool::new(false);
static TEST_PANIC_SCAN: AtomicBool = AtomicBool::new(false);
static SUMMARY: AtomicBool = AtomicBool::new(false);

unsafe fn k32(n: &[u8]) -> usize {
    static K: AtomicUsize = AtomicUsize::new(0);
    let mut m = K.load(Ordering::Relaxed);
    if m == 0 {
        let k: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
        m = GetModuleHandleW(k.as_ptr()) as usize;
        K.store(m, Ordering::Relaxed);
    }
    GetProcAddress(m as P, n.as_ptr()) as usize
}

/// True once the game's import table holds the file APIs.
unsafe fn imports_ready() -> bool {
    static WANT: OnceLock<[(usize, usize); 3]> = OnceLock::new();
    let want = WANT.get_or_init(|| [(k32(b"ReadFile\0"), 0), (k32(b"CreateFileW\0"), 0), (k32(b"CloseHandle\0"), 0)]);
    swap_imports(want, false).iter().all(|n| *n > 0)
}

const HOOK_NAMES: [&str; 8] = ["ReadFile", "CloseHandle", "SetFilePointerEx", "SetFilePointer", "GetFileSizeEx", "GetFileSize", "CreateFileW", "CreateFileA"];

/// Redirect the game's import slots. Safe to repeat: slots already
/// redirected no longer match and are left alone.
unsafe fn hook_file_io() -> Vec<usize> {
    let targets: [(&[u8], &AtomicUsize, usize); 8] = [
        (b"ReadFile\0", &O_READFILE, h_readfile as usize),
        (b"CloseHandle\0", &O_CLOSEHANDLE, h_closehandle as usize),
        (b"SetFilePointerEx\0", &O_SFPEX, h_sfpex as usize),
        (b"SetFilePointer\0", &O_SFP, h_sfp as usize),
        (b"GetFileSizeEx\0", &O_GFSEX, h_gfsex as usize),
        (b"GetFileSize\0", &O_GFS, h_gfs as usize),
        (b"CreateFileW\0", &O_CFW, h_createfilew as usize),
        (b"CreateFileA\0", &O_CFA, h_createfilea as usize),
    ];
    let mut pairs = Vec::new();
    for (name, slot, hook) in targets {
        let a = k32(name);
        slot.store(a, Ordering::SeqCst);
        pairs.push((a, hook));
    }
    swap_imports(&pairs, true)
}

unsafe fn early_file_hook() {
    let t0 = std::time::Instant::now();
    let mut placed = false;
    while t0.elapsed().as_secs() < 120 {
        if imports_ready() {
            Sleep(1); // let the loader finish writing the table
            let mut hits = hook_file_io();
            placed = hits[0] > 0;
            let ms = t0.elapsed().as_millis();
            // The loader may still be filling slots: look again for a while.
            let mut late = 0;
            for _ in 0..20 {
                Sleep(100);
                for (i, n) in hook_file_io().into_iter().enumerate() {
                    hits[i] += n;
                    late += n;
                }
            }
            let list: Vec<String> = HOOK_NAMES.iter().zip(&hits).map(|(n, c)| format!("{n}={c}")).collect();
            placed |= hits[0] > 0;
            note(&format!("file hooks installed ({ms} ms after load{}): {}", if late > 0 { format!(", {late} found later") } else { String::new() }, list.join(" ")));
            break;
        }
        // The table is normally filled within a few hundred ms; after that,
        // look less often so a failed start does not keep a core busy.
        Sleep(if t0.elapsed().as_secs() < 2 { 1 } else { 50 });
    }
    if !placed {
        note("ERROR: the game's file functions were not found, so S4HQxNHQ is inactive this session and HQ textures may look black. A game update may have changed how the game starts; please report this with this log.");
    }
    // Housekeeping tick.
    let mut last = (u64::MAX, u64::MAX, u64::MAX);
    loop {
        Sleep(15000);
        let cur = (
            N_PKG_SCANNED.load(Ordering::Relaxed) + N_PKG_CACHED.load(Ordering::Relaxed),
            N_VREADS.load(Ordering::Relaxed),
            N_DIAG.load(Ordering::Relaxed),
        );
        if cur.1 >= 200 && !CONFIRMED.swap(true, Ordering::SeqCst) {
            write_level(0); // sims are being loaded: this level works
        }
        cache::save_if_dirty();
        // Normal use: a single line once package scanning has settled.
        if !verbose() {
            if cur.0 > 0 && cur.0 == last.0 && !SUMMARY.swap(true, Ordering::SeqCst) {
                note(&format!(
                    "ready: {} packages ({} scanned, {} from cache, {} ms), {} standard-resolution sim textures will be upscaled on load",
                    cur.0,
                    N_PKG_SCANNED.load(Ordering::Relaxed),
                    N_PKG_CACHED.load(Ordering::Relaxed),
                    SCAN_MS.load(Ordering::Relaxed),
                    N_TEX.load(Ordering::Relaxed)
                ));
                if !HQ_SEEN.load(Ordering::Relaxed) {
                    match hq_setting() {
                        Some(true) => {}
                        _ => note("WARNING: the game was not seen reading GraphicsRules.sgr, so the HQ setting could not be applied automatically. If sims look broken, set RenderSimTextureSizes / CASSimTextureSize to 4096 in GraphicsRules.sgr by hand."),
                    }
                }
            }
            last = cur;
            continue;
        }
        if cur != last {
            last = cur;
            let (ws, commit) = process_memory();
            let (distinct, held_mb, held_n) = vfs::recent_stats();
            note(&format!(
                "status: packages={} ({} scanned, {} from cache, {} ms), upscalable textures={}, upscaled reads={} (RLE2 {}, RLES {}, LRLE {}, L8 {}; {} MB; avg {} us, slowest {} ms; recent results reused {} / upscaled anew {}, of those again after dropped {}, different textures {}, held {} MB in {}), game memory {} MB in use / {} MB committed, HQ setting applied={}",
                cur.0,
                N_PKG_SCANNED.load(Ordering::Relaxed),
                N_PKG_CACHED.load(Ordering::Relaxed),
                SCAN_MS.load(Ordering::Relaxed),
                N_TEX.load(Ordering::Relaxed),
                cur.1,
                N_KIND[0].load(Ordering::Relaxed),
                N_KIND[1].load(Ordering::Relaxed),
                N_KIND[2].load(Ordering::Relaxed),
                N_KIND[5].load(Ordering::Relaxed),
                N_VBYTES.load(Ordering::Relaxed) >> 20,
                UP_US.load(Ordering::Relaxed) / cur.1.max(1),
                UP_MAX_US.load(Ordering::Relaxed) / 1000,
                vfs::RECENT_HITS.load(Ordering::Relaxed),
                vfs::RECENT_MISSES.load(Ordering::Relaxed),
                vfs::RECENT_AGAIN.load(Ordering::Relaxed),
                distinct,
                held_mb,
                held_n,
                ws,
                commit,
                HQ_SEEN.load(Ordering::Relaxed)
            ));
            note(&format!(
                "  slowest read: {}; upscales at once at most {}, split across threads {} (cores {}); buffers reused {} / new {}, kept {} MB",
                lk(&UP_MAX_AT),
                pool::ACTIVE_MAX.load(Ordering::Relaxed),
                pool::SPLIT_COUNT.load(Ordering::Relaxed),
                pool::cores(),
                pool::REUSED.load(Ordering::Relaxed),
                pool::FRESH.load(Ordering::Relaxed),
                pool::kept() >> 20
            ));
            let g = lk(diag());
            let (hq, other): (Vec<_>, Vec<_>) = g.iter().partition(|(k, _)| k.starts_with("HQ "));
            if !hq.is_empty() {
                let v: Vec<String> = hq.iter().map(|(k, n)| format!("{} x{n}", &k[3..])).collect();
                note(&format!("  passed through, already HQ: {}", v.join(", ")));
            }
            if !other.is_empty() {
                let v: Vec<String> = other.iter().map(|(k, n)| format!("{k} x{n}")).collect();
                note(&format!("  passed through, other size or format: {}", v.join(", ")));
            }
        }
        if let Some(l) = frames::status() {
            note(&l);
        }
    }
}

/// Whether GraphicsRules.sgr next to the game asks for 4096 sim textures.
/// None when the file cannot be read.
fn hq_setting() -> Option<bool> {
    let d = std::fs::read(exe_sibling("GraphicsRules.sgr")).ok()?;
    let t = String::from_utf8_lossy(&d);
    Some(t.lines().any(|l| (l.contains("CASSimTextureSize") || l.contains("RenderSimTextureSizes")) && l.contains("4096")))
}

// ------------------------------------------------- d3d11.dll proxy surface

include!("table.rs");

#[no_mangle]
pub static mut HQX_TABLE: [usize; N] = [0; N];

core::arch::global_asm!(include_str!("stubs.s"));

static REAL_CD: AtomicUsize = AtomicUsize::new(0);
static REAL_CDSC: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

type CreateDevice = unsafe extern "system" fn(P, u32, P, u32, P, u32, u32, *mut P, P, *mut P) -> HRESULT;
type CreateDeviceSC = unsafe extern "system" fn(P, u32, P, u32, P, u32, u32, P, *mut P, *mut P, P, *mut P) -> HRESULT;

const E_FAIL: HRESULT = 0x80004005u32 as i32;

#[no_mangle]
pub unsafe extern "system" fn D3D11CreateDevice(a: P, b: u32, c: P, d: u32, e: P, f: u32, g: u32, dev: *mut P, fl: P, ctx: *mut P) -> HRESULT {
    let r = REAL_CD.load(Ordering::SeqCst);
    if r == 0 {
        return E_FAIL;
    }
    let o: CreateDevice = std::mem::transmute(r);
    let hr = o(a, b, c, d, e, f, g, dev, fl, ctx);
    if hr >= 0 && !dev.is_null() && ACTIVE.load(Ordering::SeqCst) && verbose() {
        frames::hook_device(*dev);
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn D3D11CreateDeviceAndSwapChain(a: P, b: u32, c: P, d: u32, e: P, f: u32, g: u32, scd: P, sc: *mut P, dev: *mut P, fl: P, ctx: *mut P) -> HRESULT {
    let r = REAL_CDSC.load(Ordering::SeqCst);
    if r == 0 {
        return E_FAIL;
    }
    let o: CreateDeviceSC = std::mem::transmute(r);
    let hr = o(a, b, c, d, e, f, g, scd, sc, dev, fl, ctx);
    if hr >= 0 && ACTIVE.load(Ordering::SeqCst) && verbose() {
        if !dev.is_null() {
            frames::hook_device(*dev);
        }
        if !sc.is_null() {
            frames::hook_swap_chain(*sc);
        }
    }
    hr
}

unsafe fn init() {
    let mut buf = [0u16; 1024];
    let n = GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), 1024) as usize;
    let exe = String::from_utf16_lossy(&buf[..n]);
    let forced = std::env::var_os("HQX_FORCE").is_some(); // test harness
    let is_game = exe.to_lowercase().ends_with("ts4_x64.exe") || forced;
    if forced && std::env::var_os("HQX_PANIC").is_some() {
        TEST_PANIC.store(true, Ordering::Relaxed);
    }
    if forced && std::env::var_os("HQX_FAULT").is_some() {
        TEST_FAULT.store(true, Ordering::Relaxed);
    }
    if forced && std::env::var_os("HQX_PANIC_SCAN").is_some() {
        TEST_PANIC_SCAN.store(true, Ordering::Relaxed);
    }
    if let Some(v) = std::env::var_os("HQX_CACHE_MAX").filter(|_| forced) {
        cache::MAX_FILE.store(v.to_string_lossy().parse().unwrap_or(8 << 20), Ordering::Relaxed);
    }
    if forced && std::env::var_os("HQX_DENSE").is_some() {
        vfs::FORCE_DENSE.store(true, Ordering::Relaxed);
    }

    let n = GetSystemDirectoryW(buf.as_mut_ptr(), 1024) as usize;
    let mut path: Vec<u16> = buf[..n].to_vec();
    path.extend("/d3d11.dll\0".encode_utf16());
    let m = LoadLibraryW(path.as_ptr());
    let mut missing = 0;
    if !m.is_null() {
        let t = (&raw mut HQX_TABLE) as *mut usize;
        for (i, name) in NAMES.iter().enumerate() {
            let a = GetProcAddress(m, name.as_ptr()) as usize;
            if a == 0 {
                missing += 1;
            }
            t.add(i).write(a);
        }
        REAL_CD.store(GetProcAddress(m, b"D3D11CreateDevice\0".as_ptr()) as usize, Ordering::SeqCst);
        REAL_CDSC.store(GetProcAddress(m, b"D3D11CreateDeviceAndSwapChain\0".as_ptr()) as usize, Ordering::SeqCst);
    }
    if is_game {
        note(&format!("--- S4HQxNHQ {VERSION}{} by Claude & AiryNyan (Fairy09.com) loaded", if cfg!(feature = "diag") { " (log build)" } else { "" }));
        note(&format!("    proxy: real_d3d11={m:p} missing_exports={missing}"));
        std::panic::set_hook(Box::new(|info| note_try(&format!("PANIC in S4HQxNHQ (recovered): {info}"))));
        ACTIVE.store(true, Ordering::SeqCst);
        AddVectoredExceptionHandler(1, veh); // crash recorder (see veh)
        if debug_file() {
            note("troubleshooting mode (S4HQxNHQ.debug present): detailed logging, crash recorder and safety levels on, scan cache off");
        } else {
            if cfg!(feature = "diag") {
                note("log-collection build: detailed logging, frame timing and crash recorder on, otherwise identical to the normal build");
            }
            cache::init(exe_sibling("S4HQxNHQ.cache"));
        }
        init_level();
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(h: P, reason: u32, _: P) -> i32 {
    if reason == 0 && ACTIVE.load(Ordering::SeqCst) {
        // Process exit: other threads are already gone and may have died
        // holding a lock, so nothing here may wait.
        write_level(0); // clean exit
        note_try(&format!("exit: {} textures upscaled this session ({} MB)", N_VREADS.load(Ordering::Relaxed), N_VBYTES.load(Ordering::Relaxed) >> 20));
    }
    if reason == 1 {
        let pe = h as usize + *((h as usize + 0x3c) as *const u32) as usize;
        OUR_BASE.store(h as usize, Ordering::SeqCst);
        OUR_SIZE.store(*((pe + 80) as *const u32) as usize, Ordering::SeqCst);
        DisableThreadLibraryCalls(h);
        init();
        if ACTIVE.load(Ordering::SeqCst) {
            std::thread::spawn(|| unsafe { early_file_hook() });
        }
    }
    1
}
