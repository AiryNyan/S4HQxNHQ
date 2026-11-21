//! Offline verification of the upscaler and package virtualization.
#[path = "../bytes.rs"]
mod bytes;
#[path = "../inflate.rs"]
mod inflate;
#[path = "../lrle.rs"]
mod lrle;
#[path = "../rle.rs"]
mod rle;
#[path = "../vfs.rs"]
mod vfs;

pub(crate) fn note(s: &str) {
    println!("NOTE {s}");
}
pub(crate) fn report(key: &str, ord: u32, size: u32) {
    println!("REPORT {key} entry {ord} size {size}");
}
pub(crate) fn lk<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

use std::io::{Read, Seek, SeekFrom};

use bytes::u16at;

/// Decode one mip to 5 bytes per pixel: R,G,B,A + the RLES extra plane.
fn decode(d: &[u8], mip: usize) -> (usize, usize, Vec<u8>) {
    let h = rle::parse_hdr(d).unwrap();
    if h.l8 {
        return decode_l8(d, &h, mip);
    }
    let bw = (((h.w >> mip).max(1)) + 3) / 4;
    let bh = (((h.h >> mip).max(1)) + 3) / 4;
    let (w, hh) = (bw * 4, bh * 4);
    let mut px = vec![0u8; w * hh * 5];
    let o = h.offs[mip];
    let (mut ca, mut ct, mut cc) = (0usize, 0usize, 0usize);
    let mut pos = 0usize;
    while pos < bw * bh {
        let cmd = u16at(d, o[0] + cc * 2);
        cc += 1;
        let (op, cnt) = (cmd & 3, cmd >> 2);
        for b in pos..pos + cnt {
            if op == 0 {
                continue;
            }
            let ep = &d[o[1] + ca * 4..o[1] + ca * 4 + 4];
            let ix = &d[o[2] + ca * 4..o[2] + ca * 4 + 4];
            let (c0, c1) = (u16at(ep, 0), u16at(ep, 2));
            let rgb = |c: usize| [((c >> 11) & 31) * 255 / 31, ((c >> 5) & 63) * 255 / 63, (c & 31) * 255 / 31];
            let (a, bb) = (rgb(c0), rgb(c1));
            let pal = [a, bb, [(2 * a[0] + bb[0]) / 3, (2 * a[1] + bb[1]) / 3, (2 * a[2] + bb[2]) / 3], [(a[0] + 2 * bb[0]) / 3, (a[1] + 2 * bb[1]) / 3, (a[2] + 2 * bb[2]) / 3]];
            let has_alpha = h.spec || op == 1;
            let ai = if h.spec { ca } else { ct };
            let mut apal = [255usize; 8];
            let mut abits = 0u64;
            if has_alpha {
                let ae = &d[o[3] + ai * 2..o[3] + ai * 2 + 2];
                let (a0, a1) = (ae[0] as usize, ae[1] as usize);
                apal[0] = a0;
                apal[1] = a1;
                if a0 > a1 {
                    for i in 1..7 {
                        apal[i + 1] = ((7 - i) * a0 + i * a1) / 7;
                    }
                } else {
                    for i in 1..5 {
                        apal[i + 1] = ((5 - i) * a0 + i * a1) / 5;
                    }
                    apal[6] = 0;
                    apal[7] = 255;
                }
                for i in 0..6 {
                    abits |= (d[o[4] + ai * 6 + i] as u64) << (8 * i);
                }
            }
            let (bx, by) = (b % bw, b / bw);
            for y in 0..4 {
                for x in 0..4 {
                    let p = ((by * 4 + y) * w + bx * 4 + x) * 5;
                    let c = pal[((ix[y] >> (2 * x)) & 3) as usize];
                    px[p] = c[0] as u8;
                    px[p + 1] = c[1] as u8;
                    px[p + 2] = c[2] as u8;
                    px[p + 3] = if has_alpha { apal[((abits >> (3 * (y * 4 + x))) & 7) as usize] as u8 } else { 255 };
                    if h.spec && op == 1 {
                        px[p + 4] = d[o[5] + ct * 16 + y * 4 + x];
                    }
                }
            }
            ca += 1;
            if op == 1 {
                ct += 1;
            }
        }
        pos += cnt;
    }
    (w, hh, px)
}

/// L8 texture: one byte per pixel, stored as 5 bytes per pixel like `decode` (value in the first).
fn decode_l8(d: &[u8], h: &rle::Hdr, mip: usize) -> (usize, usize, Vec<u8>) {
    let bw = (((h.w >> mip).max(1)) + 3) / 4;
    let bh = (((h.h >> mip).max(1)) + 3) / 4;
    let (w, hh) = (bw * 4, bh * 4);
    let mut px = vec![0u8; w * hh * 5];
    let o = h.offs[mip];
    let (mut dp, mut cc, mut pos) = (o[1], 0usize, 0usize);
    while pos < bw * bh {
        let cmd = u16at(d, o[0] + cc * 2);
        cc += 1;
        let (op, cnt) = (cmd & 3, cmd >> 2);
        for b in pos..pos + cnt {
            if op != 0 {
                // op 1: 16 raw bytes; op 2: constant block without data (shown as 255)
                let (bx, by) = (b % bw, b / bw);
                for y in 0..4 {
                    for x in 0..4 {
                        let p = ((by * 4 + y) * w + bx * 4 + x) * 5;
                        px[p] = if op == 1 { d[dp + y * 4 + x] } else { 255 };
                        px[p + 3] = 255;
                        px[p + 4] = op as u8;
                    }
                }
                if op == 1 {
                    dp += 16;
                }
            }
        }
        pos += cnt;
    }
    (w, hh, px)
}

static mut BUCKET: [f64; 4] = [0.0; 4];
static mut CNT: [usize; 4] = [0; 4];

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = std::path::Path::new(&a[1]);
    let count: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let only: Option<u8> = a.get(3).and_then(|s| s.parse().ok());
    let t0 = std::time::Instant::now();
    // HQX_BLANK=<out file>: write rle::blank() of the package's first RLES (experiment check)
    if let Some(out) = std::env::var_os("HQX_BLANK") {
        let d = std::fs::read(path).unwrap();
        let n = u32::from_le_bytes(d[36..40].try_into().unwrap()) as usize;
        let ip = u64::from_le_bytes(d[64..72].try_into().unwrap()) as usize;
        let flags = u32::from_le_bytes(d[ip..ip + 4].try_into().unwrap());
        let nc = (0..4).filter(|b| flags & (1 << b) != 0).count();
        let es = (4 - nc) * 4 + 16;
        for i in 0..n {
            let p = ip + 4 + nc * 4 + i * es + (4 - nc) * 4;
            let rd = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) as usize;
            let (off, fs, ms, comp) = (rd(p), rd(p + 4) & 0x7FFF_FFFF, rd(p + 8), u16::from_le_bytes([d[p + 12], d[p + 13]]));
            let raw = &d[off..off + fs];
            let full = if comp == 0x5A42 { inflate::inflate_sized(raw, ms + 1, ms).unwrap() } else { raw.to_vec() };
            if full.starts_with(b"DXT5RLES") {
                let b = rle::blank(&full).expect("blank");
                assert_eq!(Some(b.len()), rle::blank_len(&full[..rle::hdr_len(true, 16).min(full.len())], full.len()));
                let h = rle::parse_hdr(&b).unwrap();
                println!("blank RLES {}x{} mips {}: {} -> {} bytes, bound ok {}", h.w, h.h, h.mips, full.len(), b.len(), h.bound(b.len()).is_some());
                std::fs::write(&out, &b).unwrap();
                std::fs::write(format!("{}.orig", out.to_string_lossy()), &full).unwrap();
                return;
            }
        }
        println!("no RLES");
        return;
    }
    if std::env::var_os("HQX_DENSE").is_some() {
        vfs::FORCE_DENSE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let Ok(Some(pkg)) = vfs::Pkg::build(path, "test", None, true) else {
        println!("no sim textures / not parsable");
        return;
    };
    let k = |n: u8| pkg.ents.iter().filter(|e| e.kind == n).count();
    println!(
        "scan: {} entries (RLE2 {}, RLES {}, LRLE {}, L8 {}, specular off {}), skipped {}, dense {}, watch {}, eof {} vend {}, {} ms",
        pkg.ents.len(), k(0), k(1), k(2), k(5), k(4), pkg.skipped, pkg.dense, pkg.watch.len(), pkg.eof, pkg.vend, t0.elapsed().as_millis()
    );
    if pkg.ents.is_empty() {
        return;
    }
    // HQX_BENCH=<kind>: time the upscaler over every texture of that kind and
    // print a hash of all outputs (to prove an optimization changes nothing).
    if let Some(k) = std::env::var("HQX_BENCH").ok().and_then(|s| s.parse::<u8>().ok()) {
        let mut f = std::fs::File::open(path).unwrap();
        let (mut n, mut t_inf, mut t_up, mut hash, mut bytes) = (0u32, 0u128, 0u128, 0xcbf29ce484222325u64, 0usize);
        let mut worst = 0u128;
        for e in pkg.ents.iter().filter(|e| e.kind == k) {
            let mut raw = vec![0u8; e.fsize as usize];
            f.seek(SeekFrom::Start(e.off)).unwrap();
            f.read_exact(&mut raw).unwrap();
            let t = std::time::Instant::now();
            let full = if e.comp == 0x5A42 { inflate::inflate_sized(&raw, usize::MAX, e.msize as usize).unwrap() } else { raw };
            assert_eq!(full.len(), e.msize as usize);
            t_inf += t.elapsed().as_micros();
            let t = std::time::Instant::now();
            let up = if k == 2 { lrle::upscale(&full, Some(e.size as usize)).expect("lrle").0 } else { rle::upscale(&full).expect("rle") };
            let us = t.elapsed().as_micros();
            t_up += us;
            if us > worst && k == 2 {
                let h = lrle::parse_hdr(&full).unwrap();
                let t = std::time::Instant::now();
                let _ = lrle::decode(&full, 0);
                println!("  slow: {us} us; v2={} top level {} KB of {} KB total, out top {} KB; plain decode {} us", h.v2, h.offs[1] >> 10, full.len() >> 10, lrle::parse_hdr(&up).unwrap().offs[1] >> 10, t.elapsed().as_micros());
            }
            worst = worst.max(us);
            bytes += up.len();
            for c in up.chunks(8) {
                let mut v = [0u8; 8];
                v[..c.len()].copy_from_slice(c);
                hash = (hash ^ u64::from_le_bytes(v)).wrapping_mul(0x100000001b3);
            }
            n += 1;
        }
        println!("bench kind {k}: {n} textures, inflate avg {} us, upscale avg {} us (worst {} us), output {} MB, hash {hash:016x}", t_inf / n.max(1) as u128, t_up / n.max(1) as u128, worst, bytes >> 20);
        return;
    }
    // A rebuild from the cached plan must give the same layout.
    let t1 = std::time::Instant::now();
    let again = vfs::Pkg::build(path, "test", Some(&pkg.plan), false).ok().flatten().expect("build from cache");
    assert!(again.patches == pkg.patches && again.vend == pkg.vend && again.ents.len() == pkg.ents.len(), "cached build differs");
    println!("rebuild from cached plan: identical, {} us", t1.elapsed().as_micros());
    let mut f = std::fs::File::open(path).unwrap();
    let sel: Vec<usize> = (0..pkg.ents.len()).filter(|i| only.map_or(true, |o| pkg.ents[*i].kind == o)).collect();
    let step = (sel.len() / count).max(1);
    let (mut ok, mut up_us, mut slack) = ([0usize; 6], [0u128; 6], 0usize);
    let (mut max_ratio, mut unfit, mut max_bound) = (0f64, 0, 0usize);
    let mut adler_ok = 0usize;
    let mut refit_ok = 0usize;
    let mut blank_ok = 0usize;
    for &i in sel.iter().step_by(step) {
        let e = &pkg.ents[i];
        let mut raw = vec![0u8; e.fsize as usize];
        f.seek(SeekFrom::Start(e.off)).unwrap();
        f.read_exact(&mut raw).unwrap();
        let raw_len = raw.len();
        let raw_tail: Vec<u8> = raw[raw.len() - 4..].to_vec();
        let full = if e.comp == 0x5A42 { inflate::inflate(&raw, usize::MAX).unwrap() } else { raw };
        assert_eq!(full.len(), e.msize as usize, "inflate length");
        if e.kind == 4 {
            // specular served empty: same header, only transparent runs
            let b = rle::blank(&full).expect("blank");
            assert_eq!(b.len(), e.size as usize, "blank size");
            let h = rle::parse_hdr(&b).expect("blank header");
            assert!(h.bound(b.len()).is_some() && b[..16] == full[..16], "blank header");
            assert!(b[h.offs[0][0]..].chunks_exact(2).all(|c| c[0] & 3 == 0), "blank not transparent");
            blank_ok += 1;
            continue;
        }
        if e.comp == 0x5A42 {
            let n = raw_len;
            let want = u32::from_be_bytes([raw_tail[0], raw_tail[1], raw_tail[2], raw_tail[3]]);
            assert_eq!(inflate::adler32(&full), want, "adler32 entry {i} (fsize {n})");
            adler_ok += 1;
        }
        let t = std::time::Instant::now();
        let up = if e.kind == 2 {
            let (up, needed) = lrle::upscale(&full, Some(e.size as usize)).expect("lrle upscale");
            unfit += needed.is_some() as usize;
            // too small a size: transparent fallback, and the reported size then fits
            let (small, need) = lrle::upscale(&full, Some({ let hh = lrle::parse_hdr(&full).unwrap(); hh.base + hh.offs[hh.mips - 1] + 80 })).expect("lrle small");
            if let Some(need) = need {
                assert!(lrle::decode(&small, 0).expect("decode fallback").2.iter().all(|p| *p == 0), "fallback not transparent");
                let (fixed, n2) = lrle::upscale(&full, Some(need)).expect("lrle refit");
                assert!(n2.is_none() && fixed.len() == need, "reported size does not fit");
                assert!(lrle::decode(&fixed, 0) == lrle::decode(&up, 0), "refit differs");
                refit_ok += 1;
            }
            up
        } else {
            rle::upscale(&full).unwrap_or_else(|| panic!("upscale failed: entry {i} kind {} header {:02x?}", e.kind, &full[..16]))
        };
        up_us[e.kind as usize] += t.elapsed().as_micros();
        assert_eq!(up.len(), e.size as usize);
        max_bound = max_bound.max(up.len());
        // what the game would receive: once whole, once in two calls
        let mut got = vec![0u8; e.size as usize];
        let mut cont = None;
        assert_eq!(pkg.read_virtual(e.voff, &mut got, &mut cont).0, got.len());
        assert!(got == up && cont.is_none(), "virtual read mismatch");
        let half = got.len() / 3;
        let mut g2 = vec![0u8; got.len()];
        assert_eq!(pkg.read_virtual(e.voff, &mut g2[..half], &mut cont).0, half);
        assert!(cont.is_some());
        assert_eq!(pkg.read_virtual(e.voff + half as u64, &mut g2[half..], &mut cont).0, got.len() - half);
        assert!(g2 == up && cont.is_none(), "split virtual read mismatch");
        if e.kind == 2 {
            let h0 = lrle::parse_hdr(&full).unwrap();
            let h1 = lrle::parse_hdr(&up).unwrap();
            assert_eq!((h1.w, h1.h, h1.mips, h1.v2), (h0.w * 2, h0.h * 2, h0.mips, h0.v2));
            let ratio = h1.offs[1] as f64 / h0.offs[1].max(1) as f64;
            let bk = match h0.offs[1] { 0..=4095 => 0, 4096..=65535 => 1, 65536..=524287 => 2, _ => 3 };
            unsafe {
                if ratio > BUCKET[bk] { BUCKET[bk] = ratio; }
                CNT[bk] += 1;
            }
            if ratio > max_ratio {
                max_ratio = ratio;
            }
            let (w, hh, a0) = lrle::decode(&full, 0).expect("decode orig");
            let (w2, hh2, a1) = lrle::decode(&up, 0).expect("decode up");
            assert_eq!((w2, hh2), (w * 2, hh * 2));
            for y in 0..hh2 {
                for x in 0..w2 {
                    assert!(a1[y * w2 + x] == a0[(y / 2) * w + x / 2], "lrle pixel mismatch entry {i} at {x},{y}");
                }
            }
            for m in 0..h0.mips - 1 {
                assert!(lrle::decode(&full, m).unwrap() == lrle::decode(&up, m + 1).expect("decode up mip"), "lrle mip {m} mismatch entry {i}");
            }
            if let Some(out) = a.get(4) {
                if ok[2] == 0 {
                    let rgba = |v: &Vec<u32>| v.iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>();
                    std::fs::write(format!("{out}.lrle.orig.{w}x{hh}.bgra"), rgba(&a0)).unwrap();
                    std::fs::write(format!("{out}.lrle.up.{w2}x{hh2}.bgra"), rgba(&a1)).unwrap();
                }
            }
        } else {
            slack += up.iter().rev().take_while(|b| **b == 0).count();
            let h0 = rle::parse_hdr(&full).unwrap();
            let h1 = rle::parse_hdr(&up).unwrap();
            assert_eq!((h1.w, h1.h, h1.mips), (h0.w * 2, h0.h * 2, h0.mips + 1));
            let (w, hh, a0) = decode(&full, 0);
            let (w2, hh2, a1) = decode(&up, 0);
            assert_eq!((w2, hh2), (w * 2, hh * 2));
            for y in 0..hh2 {
                for x in 0..w2 {
                    let (p, q) = ((y * w2 + x) * 5, ((y / 2) * w + x / 2) * 5);
                    assert!(a1[p..p + 5] == a0[q..q + 5], "pixel mismatch entry {i} at {x},{y}");
                }
            }
            for m in 0..h0.mips {
                assert!(decode(&full, m) == decode(&up, m + 1), "mip {m} mismatch entry {i}");
            }
        }
        ok[e.kind as usize] += 1;
    }
    let avg = |k: usize| if ok[k] > 0 { up_us[k] / ok[k] as u128 } else { 0 };
    println!("verified pixel-exact 2x: RLE2 {} (avg {} us), RLES {} (avg {} us), LRLE {} (avg {} us), L8 {} (avg {} us)", ok[0], avg(0), ok[1], avg(1), ok[2], avg(2), ok[5], avg(5));
    if blank_ok > 0 {
        println!("specular served empty, verified: {blank_ok}");
    }
    println!("LRLE: max top-level growth x{max_ratio:.2}, did not fit: {unfit}; largest served entry {} KB; RLE avg padding {} bytes", max_bound >> 10, slack / (ok[0] + ok[1] + ok[5]).max(1));
    println!("inflate checksum verified on {adler_ok} textures; LRLE overflow-and-refit verified on {refit_ok}");
    unsafe { println!("LRLE growth by top-level size (<4K, <64K, <512K, larger): {:.2?} counts {:?}", BUCKET, CNT); }
    // transparent stand-ins (served when an upscale fails) must be valid, fully transparent textures
    for (magic, w, h, mips) in [(b"DXT5RLE2", 2048usize, 4096usize, 13usize), (b"DXT5RLES", 1024, 2048, 12), (b"L8  RLE2", 4096, 2048, 10)] {
        let t = rle::transparent(magic, w, h, mips, 4 << 20).expect("transparent rle");
        let hh = rle::parse_hdr(&t).expect("transparent rle header");
        assert!(hh.w == w && hh.h == h && hh.mips == mips && hh.bound(t.len()).is_some(), "transparent rle header");
        for m in 0..mips {
            assert!(decode(&t, m).2.chunks(5).all(|p| p[3] == 0), "transparent rle mip {m}");
        }
    }
    let t = lrle::transparent(2048, 4096, 9, 4 << 20).expect("transparent lrle");
    for m in 0..9 {
        assert!(lrle::decode(&t, m).expect("transparent lrle decode").2.iter().all(|p| *p == 0), "transparent lrle mip {m}");
    }
    println!("transparent stand-ins: valid");
    // one read covering two neighbouring virtual entries gets both
    if !pkg.dense && pkg.ents.len() >= 2 {
        let (a, b) = (&pkg.ents[0], &pkg.ents[1]);
        let mut both = vec![0u8; (a.size + b.size) as usize];
        let n = pkg.read_virtual(a.voff, &mut both, &mut None).0;
        let (mut x, mut y) = (vec![0u8; a.size as usize], vec![0u8; b.size as usize]);
        pkg.read_virtual(a.voff, &mut x, &mut None);
        pkg.read_virtual(b.voff, &mut y, &mut None);
        assert!(n == both.len() && both[..x.len()] == x[..] && both[x.len()..] == y[..], "read across entries");
        println!("read across two neighbouring entries: identical");
    }
    // several threads producing different entries at once (first time: the
    // package file is then also read through handles of their own) must give
    // the same bytes as reading them one by one
    {
        let pick: Vec<usize> = (0..pkg.ents.len()).rev().step_by((pkg.ents.len() / 24).max(1)).take(24).collect();
        let pkg_ref = &pkg;
        let together: Vec<Vec<u8>> = std::thread::scope(|sc| {
            let hs: Vec<_> = pick.iter().map(|&i| sc.spawn(move || {
                let mut v = vec![0u8; pkg_ref.ents[i].size as usize];
                pkg_ref.read_virtual(pkg_ref.ents[i].voff, &mut v, &mut None);
                v
            })).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // the same entries one by one from a second scan of the package (a
        // different Pkg, so nothing comes from the results kept for the first)
        let pkg2 = vfs::Pkg::build(path, "test2", Some(&pkg.plan), false).ok().flatten().expect("second scan");
        let alone: Vec<Vec<u8>> = pick.iter().map(|&i| {
            let mut v = vec![0u8; pkg2.ents[i].size as usize];
            pkg2.read_virtual(pkg2.ents[i].voff, &mut v, &mut None);
            v
        }).collect();
        assert!(alone == together, "concurrent reads differ");
        println!("concurrent reads of {} entries: identical", pick.len());
    }
    let mut whole = vec![0u8; pkg.idx_len];
    f.seek(SeekFrom::Start(pkg.idx_pos)).unwrap();
    f.read_exact(&mut whole).unwrap();
    let before = whole.clone();
    pkg.overlay(pkg.idx_pos, &mut whole);
    // the same index read in odd-sized pieces (also starting before and ending after it) must give the same bytes
    let lead = 777usize.min(pkg.idx_pos as usize);
    let mut big = vec![0u8; lead + pkg.idx_len];
    f.seek(SeekFrom::Start(pkg.idx_pos - lead as u64)).unwrap();
    f.read_exact(&mut big).unwrap();
    let mut o = 0usize;
    let mut step = 1usize;
    while o < big.len() {
        let n = step.min(big.len() - o);
        pkg.overlay(pkg.idx_pos - lead as u64 + o as u64, &mut big[o..o + n]);
        o += n;
        step = step * 7 % 4099 + 1;
    }
    // every patched entry must now point at its virtual entry
    let mut fields_ok = true;
    for (p, e) in pkg.patches.iter().zip(&pkg.ents) {
        let q = p.0 as usize;
        fields_ok &= u32::from_le_bytes(whole[q..q + 4].try_into().unwrap()) as u64 == e.voff && u32::from_le_bytes(whole[q + 8..q + 12].try_into().unwrap()) == e.size && whole[q + 12..q + 14] == [0, 0];
    }
    println!(
        "index overlay: {} bytes differ, fields correct: {fields_ok}, piecewise read identical: {}",
        whole.iter().zip(&before).filter(|(a, b)| a != b).count(),
        big[lead..] == whole[..]
    );
}
