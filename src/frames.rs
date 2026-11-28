//! Frame timing for the log-collection build: the time between the game's
//! presents (one per drawn frame), how often a frame took long, and whether
//! S4HQxNHQ was working during it. Hooked through the swap chain's method
//! table: the device the game creates through this d3d11.dll leads to the
//! DXGI factory, whose CreateSwapChain* hand out the swap chains.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

type P = *mut c_void;
type HRESULT = i32;

#[link(name = "kernel32")]
extern "system" {
    fn VirtualProtect(addr: P, size: usize, newp: u32, oldp: *mut u32) -> i32;
    fn GetCurrentThreadId() -> u32;
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const IID_DXGI_DEVICE: Guid = Guid(0x54ec77fa, 0x1377, 0x44e6, [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c]);
const IID_DXGI_FACTORY: Guid = Guid(0x7b7166ec, 0x21c7, 0x44ae, [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69]);
const IID_DXGI_FACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);
const IID_DXGI_SWAPCHAIN1: Guid = Guid(0x790a45f7, 0x0d42, 0x4876, [0x98, 0x3a, 0x0a, 0x55, 0xcf, 0xe6, 0xf4, 0xaa]);

// method table slots
const QI: usize = 0;
const RELEASE: usize = 2;
const GET_PARENT: usize = 6; // IDXGIObject
const GET_ADAPTER: usize = 7; // IDXGIDevice
const CREATE_SWAP_CHAIN: usize = 10; // IDXGIFactory
const CREATE_SWAP_CHAIN_FOR_HWND: usize = 15; // IDXGIFactory2
const PRESENT: usize = 8; // IDXGISwapChain
const PRESENT1: usize = 22; // IDXGISwapChain1

static O_CSC: AtomicUsize = AtomicUsize::new(0);
static O_CSCH: AtomicUsize = AtomicUsize::new(0);
static O_PRESENT: AtomicUsize = AtomicUsize::new(0);
static O_PRESENT1: AtomicUsize = AtomicUsize::new(0);
/// Swap chains hooked (status line: 0 means no frames can be seen).
pub static HOOKED: AtomicU32 = AtomicU32::new(0);
static ON: AtomicBool = AtomicBool::new(false);

unsafe fn vt(obj: P) -> *mut usize {
    *(obj as *mut *mut usize)
}

unsafe fn call_qi(obj: P, iid: &Guid) -> Option<P> {
    let f: unsafe extern "system" fn(P, *const Guid, *mut P) -> HRESULT = std::mem::transmute(*vt(obj).add(QI));
    let mut out: P = std::ptr::null_mut();
    (f(obj, iid, &mut out) >= 0 && !out.is_null()).then_some(out)
}

unsafe fn release(obj: P) {
    let f: unsafe extern "system" fn(P) -> u32 = std::mem::transmute(*vt(obj).add(RELEASE));
    f(obj);
}

/// Point `slot` of `obj`'s method table at `hook`, remembering the original
/// in `orig`. A table whose original differs from one already remembered
/// (another implementation) is left alone.
unsafe fn patch(obj: P, slot: usize, hook: usize, orig: &AtomicUsize) -> bool {
    let p = vt(obj).add(slot);
    let cur = *p;
    if cur == hook {
        return true;
    }
    if orig.compare_exchange(0, cur, Ordering::SeqCst, Ordering::SeqCst).is_err() && orig.load(Ordering::SeqCst) != cur {
        return false;
    }
    let mut old = 0u32;
    if VirtualProtect(p as P, 8, 0x04, &mut old) == 0 {
        return false;
    }
    std::ptr::write_volatile(p, hook);
    VirtualProtect(p as P, 8, old, &mut old);
    true
}

/// Called with each device the game creates (log-collection build only).
pub unsafe fn hook_device(dev: P) {
    if dev.is_null() {
        return;
    }
    ON.store(true, Ordering::Relaxed);
    let Some(dx) = call_qi(dev, &IID_DXGI_DEVICE) else { return };
    let ga: unsafe extern "system" fn(P, *mut P) -> HRESULT = std::mem::transmute(*vt(dx).add(GET_ADAPTER));
    let mut ad: P = std::ptr::null_mut();
    if ga(dx, &mut ad) >= 0 && !ad.is_null() {
        let gp: unsafe extern "system" fn(P, *const Guid, *mut P) -> HRESULT = std::mem::transmute(*vt(ad).add(GET_PARENT));
        let mut fac: P = std::ptr::null_mut();
        if gp(ad, &IID_DXGI_FACTORY, &mut fac) >= 0 && !fac.is_null() {
            patch(fac, CREATE_SWAP_CHAIN, h_csc as usize, &O_CSC);
            if let Some(f2) = call_qi(fac, &IID_DXGI_FACTORY2) {
                patch(f2, CREATE_SWAP_CHAIN_FOR_HWND, h_csch as usize, &O_CSCH);
                release(f2);
            }
            release(fac);
        }
        release(ad);
    }
    release(dx);
}

/// Called with each swap chain (log-collection build only).
pub unsafe fn hook_swap_chain(sc: P) {
    if sc.is_null() {
        return;
    }
    ON.store(true, Ordering::Relaxed);
    let mut ok = patch(sc, PRESENT, h_present as usize, &O_PRESENT);
    if let Some(s1) = call_qi(sc, &IID_DXGI_SWAPCHAIN1) {
        ok |= patch(s1, PRESENT1, h_present1 as usize, &O_PRESENT1);
        release(s1);
    }
    if ok {
        HOOKED.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "system" fn h_csc(f: P, dev: P, desc: P, sc: *mut P) -> HRESULT {
    let o: unsafe extern "system" fn(P, P, P, *mut P) -> HRESULT = std::mem::transmute(O_CSC.load(Ordering::SeqCst));
    let r = o(f, dev, desc, sc);
    if r >= 0 && !sc.is_null() {
        hook_swap_chain(*sc);
    }
    r
}

unsafe extern "system" fn h_csch(f: P, dev: P, hwnd: P, desc: P, fs: P, out: P, sc: *mut P) -> HRESULT {
    let o: unsafe extern "system" fn(P, P, P, P, P, P, *mut P) -> HRESULT = std::mem::transmute(O_CSCH.load(Ordering::SeqCst));
    let r = o(f, dev, hwnd, desc, fs, out, sc);
    if r >= 0 && !sc.is_null() {
        hook_swap_chain(*sc);
    }
    r
}

thread_local!(static IN_PRESENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

unsafe extern "system" fn h_present(sc: P, sync: u32, flags: u32) -> HRESULT {
    let o: unsafe extern "system" fn(P, u32, u32) -> HRESULT = std::mem::transmute(O_PRESENT.load(Ordering::SeqCst));
    let nested = IN_PRESENT.with(|c| c.replace(true));
    if !nested && flags & 1 == 0 {
        frame();
    }
    let r = o(sc, sync, flags);
    if !nested {
        IN_PRESENT.with(|c| c.set(false));
    }
    r
}

unsafe extern "system" fn h_present1(sc: P, sync: u32, flags: u32, params: P) -> HRESULT {
    let o: unsafe extern "system" fn(P, u32, u32, P) -> HRESULT = std::mem::transmute(O_PRESENT1.load(Ordering::SeqCst));
    let nested = IN_PRESENT.with(|c| c.replace(true));
    if !nested && flags & 1 == 0 {
        frame();
    }
    let r = o(sc, sync, flags, params);
    if !nested {
        IN_PRESENT.with(|c| c.set(false));
    }
    r
}

// ---------------------------------------------------------------- statistics

/// Frame times are kept in 1 ms steps up to this; longer gaps (window in the
/// background, game paused by the system) are counted apart.
const MAX_MS: usize = 2000;
/// Time S4HQxNHQ spent serving reads since the last frame, all threads / the drawing thread.
static WORK_US: AtomicU64 = AtomicU64::new(0);
static WORK_RENDER_US: AtomicU64 = AtomicU64::new(0);
static RENDER_TID: AtomicU32 = AtomicU32::new(0);

struct Stats {
    hist: Vec<u32>,
    frames: u64,
    sum_us: u64,
    worst_us: u64,
    worst_work_us: u64,
    over50: u32,
    over50_work: u32,
    over100: u32,
    over100_work: u32,
    paused: u32,
    work_us: u64,
    work_render_us: u64,
}

impl Stats {
    const fn new() -> Self {
        Stats { hist: Vec::new(), frames: 0, sum_us: 0, worst_us: 0, worst_work_us: 0, over50: 0, over50_work: 0, over100: 0, over100_work: 0, paused: 0, work_us: 0, work_render_us: 0 }
    }
    fn add(&mut self, us: u64, work: u64, work_render: u64) {
        self.work_us += work;
        self.work_render_us += work_render;
        let ms = (us / 1000) as usize;
        if ms > MAX_MS {
            self.paused += 1;
            return;
        }
        if self.hist.is_empty() {
            self.hist = vec![0; MAX_MS + 1];
        }
        self.hist[ms] += 1;
        self.frames += 1;
        self.sum_us += us;
        if us > self.worst_us {
            self.worst_us = us;
            self.worst_work_us = work;
        }
        if us > 50_000 {
            self.over50 += 1;
            self.over50_work += (work > 0) as u32;
        }
        if us > 100_000 {
            self.over100 += 1;
            self.over100_work += (work > 0) as u32;
        }
    }
    /// Frame time (ms) that the slowest 1% of frames reach.
    fn p99(&self) -> usize {
        let goal = self.frames.div_ceil(100);
        let mut n = 0u64;
        for ms in (0..self.hist.len()).rev() {
            n += self.hist[ms] as u64;
            if n >= goal {
                return ms;
            }
        }
        0
    }
    fn text(&self) -> String {
        let fps = if self.sum_us > 0 { self.frames as f64 * 1e6 / self.sum_us as f64 } else { 0.0 };
        format!(
            "{} frames, avg {:.1} fps, slowest 1% {} ms+, worst {} ms (S4HQxNHQ {} ms in it), over 50 ms {} (S4HQxNHQ working in {}), over 100 ms {} ({}), paused {}; S4HQxNHQ work {} ms (on the drawing thread {} ms)",
            self.frames,
            fps,
            self.p99(),
            self.worst_us / 1000,
            self.worst_work_us / 1000,
            self.over50,
            self.over50_work,
            self.over100,
            self.over100_work,
            self.paused,
            self.work_us / 1000,
            self.work_render_us / 1000
        )
    }
}

/// (since the last status line, whole session)
static STATS: Mutex<(Stats, Stats)> = Mutex::new((Stats::new(), Stats::new()));
static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn frame() {
    let now = std::time::Instant::now();
    RENDER_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    let prev = lk(&LAST).replace(now);
    let work = WORK_US.swap(0, Ordering::Relaxed);
    let work_render = WORK_RENDER_US.swap(0, Ordering::Relaxed);
    if let Some(prev) = prev {
        let us = now.duration_since(prev).as_micros() as u64;
        let mut s = lk(&STATS);
        s.0.add(us, work, work_render);
        s.1.add(us, work, work_render);
    }
}

/// A read served by S4HQxNHQ took `us` (called from the read hook).
pub fn work(us: u64) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    WORK_US.fetch_add(us, Ordering::Relaxed);
    if unsafe { GetCurrentThreadId() } == RENDER_TID.load(Ordering::Relaxed) {
        WORK_RENDER_US.fetch_add(us, Ordering::Relaxed);
    }
}

/// Status line text, or None before the hook was ever asked for.
pub fn status() -> Option<String> {
    if !ON.load(Ordering::Relaxed) {
        return None;
    }
    let mut s = lk(&STATS);
    let line = format!("  frames (swap chains hooked {}): last period {}; session {}", HOOKED.load(Ordering::Relaxed), s.0.text(), s.1.text());
    s.0 = Stats::new();
    Some(line)
}
