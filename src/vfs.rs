//! Package virtualization: presents a .package to the game with its
//! standard-resolution sim textures replaced by 2x upscaled ones, without
//! touching the file. The package index is patched in memory so those entries
//! point past the real end of file ("virtual region"); reads there are served
//! from textures upscaled on demand.

use crate::inflate::inflate;
use crate::{lrle, rle};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub const T_RLE2: u32 = 0x3453_CF95;
pub const T_RLES: u32 = 0xBA85_6C78;
pub const T_LRLE: u32 = 0x2BC0_4EDF;
pub const T_DDS: u32 = 0x00B2_D882;
const T_CASP: u32 = 0x034A_EECB;
const ZLIB: u16 = 0x5A42;
/// Highest virtual offset handed out (index offsets are 32 bit).
const VLIMIT: u64 = 0xFFFF_0000;
/// Spacing of entry offsets in dense mode (see `Pkg::dense`).
const DENSE_STEP: u64 = 16;
/// Largest upscaled texture accepted; real ones stay well below. Guards
/// against damaged packages asking for huge buffers.
const MAX_BOUND: usize = 64 << 20;

/// Safety level (debug mode): 0 = everything, 1 = no dense packages, 2 = also no LRLE.
pub static LEVEL: AtomicU8 = AtomicU8::new(0);
/// Test switch: lay every package out densely.
pub static FORCE_DENSE: AtomicBool = AtomicBool::new(false);

/// One texture to upscale as stored in the scan cache: (index entry number,
/// size of the upscaled texture, kind).
pub type Planned = (u32, u32, u8);

pub struct VEntry {
    pub voff: u64,
    pub size: u32,
    pub off: u64,
    pub fsize: u32,
    pub msize: u32,
    pub comp: u16,
    /// 0 = RLE2, 1 = RLES, 2 = LRLE, 4 = specular served empty (see `specular_off`)
    pub kind: u8,
    /// Position in the package index.
    pub ord: u32,
}

/// Progress of a read of one virtual entry that spans several ReadFile calls:
/// (entry, file position of the next call, bytes already served).
pub type Cont = Option<(usize, u64, usize)>;

pub struct Pkg {
    pub name: String,
    /// Identifies the package to `crate::report` (lowercase full path).
    pub key: String,
    pub eof: u64,
    /// File size reported to the game.
    pub vend: u64,
    pub idx_pos: u64,
    /// Size of the package index in the file.
    pub idx_len: usize,
    /// The changed index fields (offset, sizes, compression: 14 bytes) of
    /// every virtual entry, by position in the index, ascending.
    pub patches: Vec<(u32, [u8; 14])>,
    pub ents: Vec<VEntry>,
    /// Every texture found to upscale, before layout (what the cache stores).
    pub plan: Vec<Planned>,
    /// Virtual entries would not fit below 4 GB one after another, so their
    /// offsets are only DENSE_STEP apart and their byte ranges overlap. An
    /// entry is then identified by the exact offset a read starts at.
    pub dense: bool,
    pub skipped: usize,
    /// Texture entries left as they are: (offset, type, compression), sorted.
    /// Only collected in debug mode.
    pub watch: Vec<(u64, u32, u16)>,
    /// Original file offsets of the virtual entries, sorted (to notice reads
    /// that bypass the patched index).
    pub real_offs: Vec<u64>,
    /// The bypass warning for this package was logged.
    pub warned: std::sync::atomic::AtomicBool,
    path: std::path::PathBuf,
    /// Opened on the first upscale from this package, then kept.
    file: Mutex<Option<File>>,
}

/// Recently upscaled textures of all packages together, oldest first, with
/// their total size. Serves reads that fetch one texture in several calls;
/// bounded so it cannot grow with the number of packages.
static RECENT: Mutex<(VecDeque<(usize, usize, Arc<Vec<u8>>)>, usize)> = Mutex::new((VecDeque::new(), 0));
const RECENT_MAX: usize = 48 << 20;
/// Upscaled textures served from RECENT / produced anew (status log).
pub static RECENT_HITS: AtomicU64 = AtomicU64::new(0);
pub static RECENT_MISSES: AtomicU64 = AtomicU64::new(0);

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn u32at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn read_at(f: &mut File, off: u64, len: usize) -> Option<Vec<u8>> {
    let mut v = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut got = 0;
    while got < len {
        match f.read(&mut v[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    v.truncate(got);
    Some(v)
}

/// TGI of a CAS part reference: (type, instance high, instance low).
type Key = (u32, u32, u32);

/// Texture references of a CAS part (CASP, versions 43 and later):
/// (TGI list, diffuse, shadow, specular) with the latter three as list positions.
fn casp_refs(c: &[u8]) -> Option<(Vec<Key>, usize, usize, usize)> {
    let ver = u32at(c.get(..8)?, 0);
    if !(43..=64).contains(&ver) {
        return None;
    }
    let base = 8usize.checked_add(u32at(c, 4) as usize)?;
    let n = *c.get(base)? as usize;
    let mut tgis = Vec::with_capacity(n);
    for k in 0..n {
        let p = base + 1 + k * 16;
        let e = c.get(p..p + 16)?;
        let inst = u64::from_le_bytes(e[0..8].try_into().ok()?);
        tgis.push((u32at(e, 12), (inst >> 32) as u32, inst as u32));
    }
    // Before the TGI list: diffuse, shadow, composition, region map,
    // override count, overrides (5 bytes each), normal, specular,
    // UV body type (u32), emission, one more byte.
    for nov in 0..16usize {
        let Some(s) = base.checked_sub(13 + 5 * nov) else { break };
        if c[s + 4] as usize != nov {
            continue;
        }
        let (d, sh, nm, sp) = (c[s] as usize, c[s + 1] as usize, c[s + 5 + 5 * nov] as usize, c[s + 6 + 5 * nov] as usize);
        if d < n && sh < n && nm < n && sp < n {
            return Some((tgis, d, sh, sp));
        }
    }
    None
}

/// Index entries of specular maps (RLES) to serve with no specular: those of
/// CAS parts with a standard-resolution diffuse map, no shadow map and a
/// specular map already at HQ size. In HQ mode the game applies such a
/// specular map at full strength (seen as plastic-like shine: RUSTY Jalynne
/// Jogger Pants), while without HQ the part looks matte. A specular map also
/// used by a part that has a shadow map is left alone.
fn specular_off(f: &mut File, n: usize, key: &dyn Fn(usize) -> Key, fields: &dyn Fn(usize) -> (usize, u64, u32, u32, u16), eof: u64) -> std::collections::HashSet<u32> {
    use std::collections::{HashMap, HashSet};
    let mut off = HashSet::new();
    let casps: Vec<usize> = (0..n).filter(|&i| key(i).0 == T_CASP).collect();
    if casps.is_empty() {
        return off;
    }
    let by_key: HashMap<Key, usize> = (0..n).map(|i| (key(i), i)).collect();
    let mut read = |i: usize, all: bool| -> Option<Vec<u8>> {
        let (_, o, fs, ms, comp) = fields(i);
        let fs = (fs & 0x7FFF_FFFF) as usize;
        if o + fs as u64 > eof || ms as usize > 1 << 20 {
            return None;
        }
        let raw = read_at(f, o, if all { fs } else { fs.min(2048) })?;
        match comp {
            ZLIB => inflate(&raw, if all { ms as usize } else { 16 }),
            0 => Some(raw),
            _ => None,
        }
    };
    let mut keep = HashSet::new();
    let mut heads: HashMap<usize, Option<Vec<u8>>> = HashMap::new();
    for c in casps {
        let Some(d) = read(c, true) else { continue };
        let Some((tgis, di, sh, sp)) = casp_refs(&d) else { continue };
        let Some(&spi) = by_key.get(&tgis[sp]).filter(|_| sp != 0) else { continue };
        let mut head = |i: usize| heads.entry(i).or_insert_with(|| read(i, false)).clone();
        let is = |h: &Option<Vec<u8>>, magic: &[u8], w: u16, hh: u16| {
            h.as_ref().is_some_and(|h| h.len() >= 12 && h.starts_with(magic) && u16::from_le_bytes([h[8], h[9]]) == w && u16::from_le_bytes([h[10], h[11]]) == hh)
        };
        if !is(&head(spi), b"DXT5RLES", 1024, 2048) {
            continue;
        }
        let no_shadow = sh == 0 || tgis[sh] == (0, 0, 0);
        let std_diffuse = di != 0 && by_key.get(&tgis[di]).is_some_and(|&i| is(&head(i), b"DXT5RLE2", 1024, 2048));
        if no_shadow && std_diffuse {
            off.insert(spi as u32);
        } else {
            keep.insert(spi as u32);
        }
    }
    off.retain(|i| !keep.contains(i));
    off
}

/// Size of the upscaled texture if `head` starts a standard-resolution sim
/// texture. Decided by the data itself, not the resource type: some packages
/// store RLE2 data under the LRLE type.
fn plan(head: &[u8], msize: usize) -> Option<(usize, u8)> {
    if head.starts_with(b"LRLE") {
        let h = lrle::parse_hdr(head)?;
        if !h.needs_upscale() {
            return None;
        }
        Some((h.bound(msize)?, 2))
    } else {
        let h = rle::parse_hdr(head)?;
        if !h.needs_upscale() {
            return None;
        }
        Some((h.bound(msize)?, h.spec as u8))
    }
}

/// Short description of a texture resource from its first bytes (diagnostics).
pub fn describe(ty: u32, data: &[u8], comp: u16) -> String {
    let head = if comp == ZLIB { inflate(data, 160).unwrap_or_default() } else { data[..data.len().min(160)].to_vec() };
    let u16at = |o: usize| u16::from_le_bytes([head[o], head[o + 1]]);
    if head.len() >= 16 && &head[0..4] == b"DXT5" {
        let (kind, w, h) = (String::from_utf8_lossy(&head[4..8]).into_owned(), u16at(8), u16at(10));
        let hq = (kind == "RLE2" && (w, h) == (2048, 4096)) || (kind == "RLES" && (w, h) == (1024, 2048));
        return format!("{}{kind} {w}x{h}", if hq { "HQ " } else { "" });
    }
    if head.len() >= 16 && &head[0..4] == b"LRLE" {
        let (w, h) = (u16at(8), u16at(10));
        return format!("{}LRLE{} {w}x{h}", if (w, h) == (2048, 4096) { "HQ " } else { "" }, if head[4] == 0 { "v1" } else { "v2" });
    }
    if head.len() >= 88 && &head[0..4] == b"DDS " {
        let (h, w) = (u32at(&head, 12), u32at(&head, 16));
        // Only sim-shaped (1:2) images are interesting; the rest are object textures.
        if h >= 512 && h / 2 == w {
            return format!("DDS/{} {w}x{h}", String::from_utf8_lossy(&head[84..88]).trim_end_matches('\0'));
        }
        return "DDS other".into();
    }
    let magic: String = head.iter().take(8).map(|b| if b.is_ascii_graphic() { *b as char } else { '.' }).collect();
    format!("type {ty:08X} unknown (starts with \"{magic}\")")
}

impl Pkg {
    /// Scan a package. Returns None when it holds nothing of interest or
    /// cannot be parsed; such packages are left untouched.
    ///
    /// `cached`: the result of an earlier scan of the same file; texture
    /// headers are then not read again. `want_watch`: also list the sim
    /// textures that are not upscaled (debug mode).
    pub fn build(path: &Path, key: &str, cached: Option<&[Planned]>, want_watch: bool) -> Option<Pkg> {
        let mut f = File::open(path).ok()?;
        let eof = f.metadata().ok()?.len();
        let h = read_at(&mut f, 0, 96)?;
        if h.len() < 96 || &h[0..4] != b"DBPF" || u32at(&h, 4) != 2 {
            return None;
        }
        let n = u32at(&h, 36) as usize;
        let mut idx_pos = u64::from_le_bytes(h[64..72].try_into().ok()?);
        if idx_pos == 0 {
            idx_pos = u32at(&h, 40) as u64;
        }
        let idx_size = u32at(&h, 44) as usize;
        if n == 0 || idx_pos == 0 || idx_pos + idx_size as u64 > eof {
            return None;
        }
        let idx = read_at(&mut f, idx_pos, idx_size)?;
        if idx.len() < 4 {
            return None;
        }
        let flags = u32at(&idx, 0);
        let mut o = 4usize;
        let mut konst = [None::<u32>; 4];
        for (b, k) in konst.iter_mut().enumerate() {
            // bit0 type, bit1 group, bit2 instance-high, bit3 instance-low
            if flags & (1 << b) != 0 {
                if o + 4 > idx.len() {
                    return None;
                }
                *k = Some(u32at(&idx, o));
                o += 4;
            }
        }
        let var_fields = konst.iter().filter(|k| k.is_none()).count();
        let esize = var_fields * 4 + 16;
        if n.checked_mul(esize).and_then(|x| x.checked_add(o)).map_or(true, |end| end > idx.len()) {
            return None;
        }
        let is_tex = |t: u32| t == T_RLE2 || t == T_RLES || t == T_LRLE || t == T_DDS;
        if let Some(t) = konst[0] {
            if !is_tex(t) {
                return None;
            }
        }
        // Index fields of entry `i`: (position of the offset field, offset, raw file size, memory size, compression)
        let fields = |i: usize| {
            let p = o + i * esize + var_fields * 4;
            (p, u32at(&idx, p) as u64, u32at(&idx, p + 4), u32at(&idx, p + 8), u16::from_le_bytes([idx[p + 12], idx[p + 13]]))
        };
        let usable = |off: u64, fsize: u32, comp: u16| (comp == ZLIB || comp == 0) && fsize >= 16 && off + fsize as u64 <= eof;
        // (index position, raw file size, entry without voff)
        let mut cand: Vec<(usize, u32, VEntry)> = Vec::new();
        let mut watch = Vec::new();
        let level = LEVEL.load(Ordering::Relaxed);
        if let Some(list) = cached {
            for &(ord, bound, kind) in list {
                if ord as usize >= n || !matches!(kind, 0 | 1 | 2 | 4) || bound as usize > MAX_BOUND {
                    return None; // cache does not match this file
                }
                let (p, off, fs_raw, msize, comp) = fields(ord as usize);
                let fsize = fs_raw & 0x7FFF_FFFF;
                if !usable(off, fsize, comp) {
                    return None;
                }
                cand.push((p, fs_raw, VEntry { voff: 0, size: bound, off, fsize, msize, comp, kind, ord }));
            }
            if want_watch {
                // every other texture entry is one that stays as it is
                let planned: std::collections::HashSet<u32> = list.iter().map(|e| e.0).collect();
                for i in 0..n {
                    let ty = konst[0].unwrap_or_else(|| u32at(&idx, o + i * esize));
                    let (_, off, fs_raw, _, comp) = fields(i);
                    if is_tex(ty) && !planned.contains(&(i as u32)) && usable(off, fs_raw & 0x7FFF_FFFF, comp) {
                        watch.push((off, ty, comp));
                    }
                }
            }
        } else {
            let key = |i: usize| -> Key {
                let mut v = [0u32; 4];
                let mut q = o + i * esize;
                for (b, k) in konst.iter().enumerate() {
                    v[b] = k.unwrap_or_else(|| {
                        q += 4;
                        u32at(&idx, q - 4)
                    });
                }
                (v[0], v[2], v[3])
            };
            let spec_off = specular_off(&mut f, n, &key, &fields, eof);
            for i in 0..n {
                let ty = konst[0].unwrap_or_else(|| u32at(&idx, o + i * esize));
                if !is_tex(ty) {
                    continue;
                }
                let (p, off, fs_raw, msize, comp) = fields(i);
                let fsize = fs_raw & 0x7FFF_FFFF;
                if !usable(off, fsize, comp) {
                    continue;
                }
                let mut planned = None;
                if ty != T_DDS {
                    // Peek at the texture header only.
                    let need = rle::hdr_len(true, 16).max(lrle::PEEK);
                    if let Some(raw) = read_at(&mut f, off, (fsize as usize).min(2048)) {
                        let head = if comp == ZLIB { inflate(&raw, need) } else { Some(raw) };
                        planned = head.as_deref().and_then(|h| plan(h, msize as usize)).filter(|p| !(p.1 == 2 && level >= 2));
                        if planned.is_none() && spec_off.contains(&(i as u32)) {
                            planned = head.as_deref().filter(|h| h.starts_with(b"DXT5RLES")).and_then(|h| rle::blank_len(h, msize as usize)).map(|n| (n, 4));
                        }
                    }
                }
                match planned {
                    Some((bound, kind)) if bound <= MAX_BOUND => cand.push((p, fs_raw, VEntry { voff: 0, size: bound as u32, off, fsize, msize, comp, kind, ord: i as u32 })),
                    _ if want_watch => watch.push((off, ty, comp)),
                    _ => {}
                }
            }
        }
        let plan: Vec<Planned> = cand.iter().map(|c| (c.2.ord, c.2.size, c.2.kind)).collect();
        if cand.is_empty() && watch.is_empty() {
            return None;
        }
        let base = (eof + 0xFFFF) & !0xFFFF;
        let sum: u64 = cand.iter().map(|c| c.2.size as u64).sum();
        let max = cand.iter().map(|c| c.2.size as u64).max().unwrap_or(0);
        let dense = level < 1 && (base + sum > VLIMIT || FORCE_DENSE.load(Ordering::Relaxed));
        let mut skipped = 0;
        if dense && base + cand.len() as u64 * DENSE_STEP + max > VLIMIT {
            // Not even dense offsets fit below 4 GB.
            skipped = cand.len();
            if want_watch {
                watch.extend(cand.iter().map(|c| (c.2.off, 0, c.2.comp)));
            }
            cand.clear();
        }
        let idx_len = idx.len();
        drop(idx);
        let mut patches: Vec<(u32, [u8; 14])> = Vec::with_capacity(cand.len());
        let mut ents = Vec::with_capacity(cand.len());
        let mut vcur = base;
        for (p, fs_raw, mut e) in cand {
            if !dense && vcur + e.size as u64 > VLIMIT {
                skipped += 1;
                if want_watch {
                    watch.push((e.off, 0, e.comp));
                }
                continue;
            }
            e.voff = vcur;
            vcur += if dense { DENSE_STEP } else { e.size as u64 };
            let mut f = [0u8; 14];
            f[0..4].copy_from_slice(&(e.voff as u32).to_le_bytes());
            f[4..8].copy_from_slice(&(e.size | (fs_raw & 0x8000_0000)).to_le_bytes());
            f[8..12].copy_from_slice(&e.size.to_le_bytes());
            // f[12..14]: compression = none
            patches.push((p as u32, f));
            ents.push(e);
        }
        watch.sort_unstable();
        patches.sort_unstable_by_key(|p| p.0);
        let mut real_offs: Vec<u64> = ents.iter().map(|e| e.off).collect();
        real_offs.sort_unstable();
        drop(f); // most packages are never read from by the upscaler
        let vend = if ents.is_empty() {
            eof
        } else if dense {
            vcur + max
        } else {
            vcur
        };
        Some(Pkg {
            name: path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            key: key.to_string(),
            eof,
            vend,
            idx_pos,
            idx_len,
            patches,
            ents,
            plan,
            dense,
            skipped,
            watch,
            real_offs,
            warned: AtomicBool::new(false),
            path: path.to_path_buf(),
            file: Mutex::new(None),
        })
    }

    /// Replace index bytes in a buffer the game just read from `pos`.
    pub fn overlay(&self, pos: u64, buf: &mut [u8]) {
        if self.ents.is_empty() {
            return;
        }
        let (a, b) = (pos.max(self.idx_pos), (pos + buf.len() as u64).min(self.idx_pos + self.idx_len as u64));
        if a >= b {
            return;
        }
        // the part of the index this read covers, as offsets within the index
        let (ra, rb) = (a - self.idx_pos, b - self.idx_pos);
        let first = self.patches.partition_point(|p| p.0 as u64 + 14 <= ra);
        for (off, bytes) in &self.patches[first..] {
            let off = *off as u64;
            if off >= rb {
                break;
            }
            let (s, e) = (off.max(ra), (off + 14).min(rb));
            let dst = (s + self.idx_pos - pos) as usize;
            buf[dst..dst + (e - s) as usize].copy_from_slice(&bytes[(s - off) as usize..(e - off) as usize]);
        }
    }

    /// True when `pos` is where the original data of a texture that should
    /// have been served upscaled starts: the game is using the unpatched index.
    pub fn bypassed(&self, pos: u64) -> bool {
        self.real_offs.binary_search(&pos).is_ok()
    }

    /// Type and compression of an untouched texture entry starting at `pos`.
    pub fn watched(&self, pos: u64) -> Option<(u32, u16)> {
        let i = self.watch.binary_search_by(|w| w.0.cmp(&pos)).ok()?;
        Some((self.watch[i].1, self.watch[i].2))
    }

    fn produce(&self, i: usize) -> Arc<Vec<u8>> {
        let id = self as *const Pkg as usize;
        if let Some((_, _, v)) = lk(&RECENT).0.iter().find(|(p, k, _)| *p == id && *k == i) {
            RECENT_HITS.fetch_add(1, Ordering::Relaxed);
            return v.clone();
        }
        RECENT_MISSES.fetch_add(1, Ordering::Relaxed);
        let e = &self.ents[i];
        let data = (|| {
            let raw = {
                let mut g = lk(&self.file);
                if g.is_none() {
                    *g = Some(File::open(&self.path).ok()?);
                }
                read_at(g.as_mut()?, e.off, e.fsize as usize)?
            };
            // Stop at the size the index promises: a damaged stream must not
            // grow without limit (running out of memory ends the game).
            let full = if e.comp == ZLIB { crate::inflate::inflate_sized(&raw, e.msize as usize + 1, e.msize as usize)? } else { raw };
            if full.len() != e.msize as usize {
                return None;
            }
            let up = if e.kind == 4 {
                rle::blank(&full)?
            } else if e.kind == 2 {
                let (up, needed) = lrle::upscale(&full, Some(e.size as usize))?;
                if let Some(needed) = needed {
                    crate::note(&format!("LRLE too complex, served transparent (fixed from the next start): {} entry {}", self.name, e.ord));
                    crate::report(&self.key, e.ord, needed as u32);
                }
                up
            } else {
                rle::upscale(&full)?
            };
            (up.len() == e.size as usize).then_some(up)
        })();
        let ok = data.is_some();
        let v = Arc::new(data.unwrap_or_else(|| vec![0u8; e.size as usize]));
        if !ok {
            crate::note(&format!("UPSCALE FAILED {} entry {} kind={} off={}", self.name, e.ord, e.kind, e.off));
            crate::report(&self.key, e.ord, 0);
        }
        let mut c = lk(&RECENT);
        c.1 += v.len();
        c.0.push_back((id, i, v.clone()));
        while c.1 > RECENT_MAX && c.0.len() > 1 {
            if let Some((_, _, old)) = c.0.pop_front() {
                c.1 -= old.len();
            }
        }
        v
    }

    /// Serve a read that starts in the virtual region. Returns bytes written
    /// and the kind of texture served (3 = none).
    pub fn read_virtual(&self, pos: u64, buf: &mut [u8], cont: &mut Cont) -> (usize, u8) {
        let prev = cont.take();
        if buf.is_empty() || pos >= self.vend || self.ents.is_empty() {
            return (0, 3);
        }
        let (i, within) = match prev {
            Some((i, next, within)) if next == pos => (i, within),
            _ => match self.ents.binary_search_by(|e| e.voff.cmp(&pos)) {
                Ok(i) => (i, 0),
                Err(0) => {
                    // gap between real EOF and the first entry: zeros
                    let n = ((self.ents[0].voff - pos) as usize).min(buf.len());
                    buf[..n].fill(0);
                    return (n, 3);
                }
                Err(i) => {
                    if self.dense {
                        crate::note(&format!("UNEXPECTED read inside dense entry: {} pos={pos} len={}", self.name, buf.len()));
                    }
                    (i - 1, (pos - self.ents[i - 1].voff) as usize)
                }
            },
        };
        let e = &self.ents[i];
        if within >= e.size as usize {
            return (0, 3);
        }
        let n = (e.size as usize - within).min(buf.len());
        let data = self.produce(i);
        buf[..n].copy_from_slice(&data[within..within + n]);
        if within + n < e.size as usize {
            *cont = Some((i, pos + n as u64, within + n));
        }
        (n, e.kind)
    }
}
