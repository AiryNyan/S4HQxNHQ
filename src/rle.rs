//! Sims 4 RLE2 / RLES / L8 sim-texture container: header parsing and an
//! exact 2x upscale done in the compressed (DXT block) domain.
//!
//! Layout: 16-byte header, then per mip K u32 offsets (K=5 for RLE2, 6 for
//! RLES, 2 for "L8  RLE2": single-channel pet textures, 16 raw bytes per
//! op-1 block; op-2 blocks have no data). The file body is K columns; column j holds stream j of every mip
//! back to back. Column 0 is the u16 command stream (op = low 2 bits, count
//! = high 14 bits; op 0 = transparent run, 1 = translucent, 2 = opaque).

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Copy, // per-block constants (endpoints)
    Idx2, // 16 texels x 2 bits
    Idx3, // 16 texels x 3 bits
    Raw,  // 16 texels x 8 bits
}

pub struct Hdr {
    pub spec: bool,
    /// "L8  RLE2": one 8-bit channel, raw 4x4 bytes per block (pets).
    pub l8: bool,
    pub w: usize,
    pub h: usize,
    pub mips: usize,
    pub k: usize,
    pub offs: Vec<[usize; 6]>,
}

/// (bytes per block, kind, present for opaque blocks too)
pub fn streams(spec: bool, l8: bool) -> &'static [(usize, Kind, bool)] {
    if l8 {
        // only op-1 blocks carry pixels; op-2 blocks are a constant without data
        &[(16, Kind::Raw, false)]
    } else if spec {
        &[(4, Kind::Copy, true), (4, Kind::Idx2, true), (2, Kind::Copy, true), (6, Kind::Idx3, true), (16, Kind::Raw, false)]
    } else {
        &[(4, Kind::Copy, true), (4, Kind::Idx2, true), (2, Kind::Copy, false), (6, Kind::Idx3, false)]
    }
}

/// Header length of an RLE2 (`spec` false) or RLES (`spec` true) texture.
pub fn hdr_len(spec: bool, mips: usize) -> usize {
    hdr_len_k(if spec { 6 } else { 5 }, mips)
}

fn hdr_len_k(k: usize, mips: usize) -> usize {
    16 + mips * 4 * k
}

use crate::bytes::{u16at, u32at};

/// Parse the header; `d` may be just the first bytes of the resource.
pub fn parse_hdr(d: &[u8]) -> Option<Hdr> {
    if d.len() < 16 {
        return None;
    }
    let (spec, l8, k) = match &d[0..8] {
        b"DXT5RLE2" => (false, false, 5),
        b"DXT5RLES" => (true, false, 6),
        b"L8  RLE2" => (false, true, 2),
        _ => return None,
    };
    let (w, h, mips) = (u16at(d, 8), u16at(d, 10), u16at(d, 12));
    if mips == 0 || mips > 16 || d.len() < hdr_len_k(k, mips) {
        return None;
    }
    let mut offs = Vec::with_capacity(mips);
    for i in 0..mips {
        let mut o = [0usize; 6];
        for (j, v) in o.iter_mut().enumerate().take(k) {
            *v = u32at(d, 16 + (i * k + j) * 4);
        }
        offs.push(o);
    }
    Some(Hdr { spec, l8, w, h, mips, k, offs })
}

impl Hdr {
    /// End of column j (all mips) given the total resource length.
    fn col_end(&self, j: usize, total: usize) -> usize {
        if j + 1 < self.k { self.offs[0][j + 1] } else { total }
    }
    /// Size of mip 0's part of column j.
    fn m0(&self, j: usize, total: usize) -> usize {
        let end = if self.mips > 1 { self.offs[1][j] } else { self.col_end(j, total) };
        end.wrapping_sub(self.offs[0][j])
    }
    fn sane(&self, total: usize) -> bool {
        if self.w % 4 != 0 || self.h % 4 != 0 || self.offs[0][0] != hdr_len_k(self.k, self.mips) {
            return false;
        }
        for j in 0..self.k {
            let (s, e) = (self.offs[0][j], self.col_end(j, total));
            if s > e || e > total {
                return false;
            }
            for i in 0..self.mips {
                let o = self.offs[i][j];
                if o < s || o > e || (i > 0 && o < self.offs[i - 1][j]) {
                    return false;
                }
            }
        }
        true
    }
    /// True for a standard-resolution CAS texture that the HQ compositor
    /// cannot use as is: sims are portrait (1024x2048, specular 512x1024),
    /// pets and horses landscape (2048x1024, specular 1024x512).
    pub fn needs_upscale(&self) -> bool {
        let wh = (self.w, self.h);
        if self.spec { wh == (512, 1024) || wh == (1024, 512) } else { wh == (1024, 2048) || wh == (2048, 1024) }
    }
    /// Upper bound of `upscale()` output length, computable from the header alone.
    pub fn bound(&self, total: usize) -> Option<usize> {
        if !self.sane(total) {
            return None;
        }
        let mut n = hdr_len_k(self.k, self.mips + 1) + (total - hdr_len_k(self.k, self.mips));
        n += 4 * self.m0(0, total) + self.h + 256; // commands (see upscale)
        for j in 1..self.k {
            n += 4 * self.m0(j, total);
        }
        Some(n)
    }
}

/// Doubling of a 2x2 quadrant of 3-bit indices into a 4x4 block, by lookup.
/// Key: the two source texels of the upper row (6 bits), then the lower row.
fn idx3_table() -> &'static [u64] {
    static T: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        (0..4096u64)
            .map(|q| {
                let mut o = 0u64;
                for y in 0..4 {
                    for x in 0..4 {
                        let s = (y / 2) * 6 + (x / 2) * 3; // bit position of the source texel in q
                        o |= ((q >> s) & 7) << (3 * (y * 4 + x));
                    }
                }
                o
            })
            .collect()
    })
}

/// Doubling of two 2-bit indices (a nibble of a source row) into one output row byte.
const IDX2: [u8; 16] = {
    let mut t = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        let (a, b) = (i as u8 & 3, (i as u8 >> 2) & 3);
        t[i] = a | (a << 2) | (b << 4) | (b << 6);
        i += 1;
    }
    t
};

/// Write the doubled elements of a range of source block rows into `out`
/// (exactly `4 * n * B` bytes for `n` source elements). The elements of one
/// stream that belong to one block row are contiguous in the source, so a row
/// is doubled by walking that slice: each source row yields two output rows
/// and each element of `B` bytes two neighbouring elements in each of them.
/// `pre` holds the element number at the start of each row (and one past the
/// last), `src` the elements from `pre[0]` on. `f` maps a source element to
/// the element for quadrant (qx, qy).
#[inline]
fn fill<const B: usize>(src: &[u8], pre: &[u32], out: &mut [u8], f: impl Fn(&[u8; B], usize, usize) -> [u8; B]) {
    let base = pre[0] as usize;
    let mut w = 0;
    for r in pre.windows(2) {
        let row = &src[(r[0] as usize - base) * B..(r[1] as usize - base) * B];
        let n = row.len() / B;
        for qy in 0..2 {
            for (o, c) in out[w..w + 2 * B * n].chunks_exact_mut(2 * B).zip(row.chunks_exact(B)) {
                let c: &[u8; B] = c.try_into().unwrap();
                o[..B].copy_from_slice(&f(c, 0, qy));
                o[B..].copy_from_slice(&f(c, 1, qy));
            }
            w += 2 * B * n;
        }
    }
}

/// Double one stream over a range of block rows (see `fill`) into `out`,
/// which holds exactly its size.
fn double_stream(src: &[u8], pre: &[u32], bytes: usize, kind: Kind, out: &mut [u8]) -> Option<()> {
    let n = (pre[pre.len() - 1] - pre[0]) as usize;
    if out.len() != 4 * n * bytes || src.len() != n * bytes {
        return None;
    }
    match (kind, bytes) {
        (Kind::Copy, 4) => fill::<4>(src, pre, out, |v, _, _| *v),
        (Kind::Copy, 2) => fill::<2>(src, pre, out, |v, _, _| *v),
        (Kind::Idx2, 4) => fill::<4>(src, pre, out, |v, qx, qy| {
            let (r0, r1) = (IDX2[((v[qy * 2] >> (qx * 4)) & 15) as usize], IDX2[((v[qy * 2 + 1] >> (qx * 4)) & 15) as usize]);
            [r0, r0, r1, r1]
        }),
        (Kind::Idx3, 6) => {
            let t3 = idx3_table();
            fill::<6>(src, pre, out, |v, qx, qy| {
                let x = u64::from_le_bytes([v[0], v[1], v[2], v[3], v[4], v[5], 0, 0]);
                let pos = 3 * (qy * 8 + qx * 2);
                let q = ((x >> pos) & 63) | (((x >> (pos + 12)) & 63) << 6);
                let o = t3[q as usize].to_le_bytes();
                [o[0], o[1], o[2], o[3], o[4], o[5]]
            })
        }
        (Kind::Raw, 16) => fill::<16>(src, pre, out, |v, qx, qy| {
            let mut o = [0u8; 16];
            for y in 0..4 {
                let r = (qy * 2 + y / 2) * 4 + qx * 2;
                o[y * 4] = v[r];
                o[y * 4 + 1] = v[r];
                o[y * 4 + 2] = v[r + 1];
                o[y * 4 + 3] = v[r + 1];
            }
            o
        }),
        _ => return None,
    }
    Some(())
}

/// Size of `blank()`'s output, from the header alone.
pub fn blank_len(d: &[u8], total: usize) -> Option<usize> {
    let h = parse_hdr(d)?;
    if !h.sane(total) {
        return None;
    }
    Some(hdr_len_k(h.k, h.mips) + (h.offs[0][1] - h.offs[0][0]))
}

/// The same texture with every block transparent (same size and
/// mip count, all commands turned into transparent runs, no block data).
pub fn blank(d: &[u8]) -> Option<Vec<u8>> {
    let h = parse_hdr(d)?;
    let total = d.len();
    if !h.sane(total) {
        return None;
    }
    let hl = hdr_len_k(h.k, h.mips);
    let (c0, c1) = (h.offs[0][0], h.offs[0][1]);
    let mut out = d[..hl].to_vec();
    for i in 0..h.mips {
        for j in 0..h.k {
            let v = if j == 0 { hl + (h.offs[i][0] - c0) } else { hl + (c1 - c0) };
            out[16 + (i * h.k + j) * 4..][..4].copy_from_slice(&(v as u32).to_le_bytes());
        }
    }
    for c in d[c0..c1].chunks_exact(2) {
        let v = u16::from_le_bytes([c[0], c[1]]) & !3;
        out.extend_from_slice(&v.to_le_bytes());
    }
    Some(out)
}

/// A valid, fully transparent texture of the given size and mip count:
/// header, transparent-run commands for every level, empty data streams.
/// Served in place of an upscale that failed, so the game never receives
/// bytes that are not a texture. Padded with zeros to `size`.
pub fn transparent(magic: &[u8; 8], w: usize, h: usize, mips: usize, size: usize) -> Option<Vec<u8>> {
    if mips == 0 || mips > 16 || w == 0 || h == 0 {
        return None;
    }
    let k = match magic {
        b"DXT5RLE2" => 5,
        b"DXT5RLES" => 6,
        b"L8  RLE2" => 2,
        _ => return None,
    };
    let hl = hdr_len_k(k, mips);
    let mut cmds: Vec<u8> = Vec::new();
    let mut starts = Vec::with_capacity(mips);
    for i in 0..mips {
        starts.push(hl + cmds.len());
        let blocks = ((w >> i).max(1)).div_ceil(4) * ((h >> i).max(1)).div_ceil(4);
        let mut left = blocks;
        while left > 0 {
            let n = left.min(0x3FFF);
            cmds.extend_from_slice(&((n as u16) << 2).to_le_bytes());
            left -= n;
        }
    }
    let end = hl + cmds.len();
    if end > size {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(magic);
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.extend_from_slice(&(mips as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    for s in &starts {
        out.extend_from_slice(&(*s as u32).to_le_bytes());
        for _ in 1..k {
            out.extend_from_slice(&(end as u32).to_le_bytes());
        }
    }
    out.extend_from_slice(&cmds);
    out.resize(size, 0);
    Some(out)
}

/// Exact 2x (pixel-doubling) upscale. The result has one more mip level: the
/// new top level is generated, all original levels are reused unchanged.
/// Output is padded with zeros to `bound()` so its size is known in advance.
/// Large textures are doubled on several threads (see `pool::threads_for`).
pub fn upscale(d: &[u8]) -> Option<Vec<u8>> {
    let h = parse_hdr(d)?;
    let total = d.len();
    let bound = h.bound(total)?;
    let st = streams(h.spec, h.l8);
    let (bw, bh) = (h.w / 4, h.h / 4);
    let nblk = bw * bh;
    if nblk == 0 {
        return None;
    }

    // Mip 0 commands as runs cut at block row ends, and per block row the
    // number of non-transparent (pa) and translucent (pt) blocks before it:
    // the element numbers where the row starts in the streams.
    let ncmd = h.m0(0, total) / 2;
    let mut seg: Vec<(u8, u32)> = Vec::with_capacity(ncmd + bh);
    let mut seg_at: Vec<u32> = Vec::with_capacity(bh + 1);
    seg_at.push(0);
    let (mut pa, mut pt) = (vec![0u32; bh + 1], vec![0u32; bh + 1]);
    let (mut pos, mut na, mut nt) = (0usize, 0u32, 0u32);
    for c in 0..ncmd {
        let cmd = u16at(d, h.offs[0][0] + c * 2);
        let (op, cnt) = (cmd & 3, cmd >> 2);
        if op == 3 || pos + cnt > nblk {
            return None;
        }
        let mut left = cnt;
        while left > 0 {
            let n = left.min(bw - pos % bw);
            seg.push((op as u8, n as u32));
            pos += n;
            left -= n;
            if op != 0 {
                na += n as u32;
            }
            if op == 1 {
                nt += n as u32;
            }
            if pos % bw == 0 {
                seg_at.push(seg.len() as u32);
                pa[pos / bw] = na;
                pt[pos / bw] = nt;
            }
        }
    }
    if pos != nblk {
        return None;
    }
    for (j, (bytes, _, both)) in st.iter().enumerate() {
        let n = if *both { na } else { nt } as usize;
        if h.m0(j + 1, total) != n * bytes {
            return None;
        }
    }

    // New mip 0 commands: every source block row twice, every run doubled.
    let mut cmds: Vec<u8> = Vec::with_capacity(4 * ncmd * 2 + 64);
    let (mut run_op, mut run_len) = (seg[0].0, 0usize);
    let flush = |op: u8, mut len: usize, cmds: &mut Vec<u8>| {
        while len > 0 {
            let n = len.min(0x3FFF);
            cmds.extend_from_slice(&(((n as u16) << 2) | op as u16).to_le_bytes());
            len -= n;
        }
    };
    for oy in 0..bh * 2 {
        for &(op, n) in &seg[seg_at[oy / 2] as usize..seg_at[oy / 2 + 1] as usize] {
            if op != run_op {
                flush(run_op, run_len, &mut cmds);
                run_op = op;
                run_len = 0;
            }
            run_len += 2 * n as usize;
        }
    }
    flush(run_op, run_len, &mut cmds);

    // Layout: new header, then each column = new mip 0 part + whole old
    // column. Everything but the new mip 0 streams is placed first.
    let nh = hdr_len_k(h.k, h.mips + 1);
    let mut out = crate::pool::take(bound);
    out[0..8].copy_from_slice(&d[0..8]);
    out[8..10].copy_from_slice(&((h.w * 2) as u16).to_le_bytes());
    out[10..12].copy_from_slice(&((h.h * 2) as u16).to_le_bytes());
    out[12..14].copy_from_slice(&((h.mips + 1) as u16).to_le_bytes());
    out[14..16].copy_from_slice(&d[14..16]);
    let mut regions: Vec<(usize, usize)> = Vec::with_capacity(h.k); // (start, stream) of each new mip 0 stream
    let mut cur = nh;
    for j in 0..h.k {
        let (os, oe) = (h.offs[0][j], h.col_end(j, total));
        let newlen = if j == 0 {
            cmds.len()
        } else {
            let (bytes, _, both) = st[j - 1];
            4 * if both { na } else { nt } as usize * bytes
        };
        if cur + newlen + (oe - os) > bound {
            return None;
        }
        let put = |out: &mut Vec<u8>, mip: usize, v: usize| {
            let o = 16 + (mip * h.k + j) * 4;
            out[o..o + 4].copy_from_slice(&(v as u32).to_le_bytes());
        };
        put(&mut out, 0, cur);
        for i in 0..h.mips {
            put(&mut out, i + 1, cur + newlen + (h.offs[i][j] - os));
        }
        if j == 0 {
            out[cur..cur + newlen].copy_from_slice(&cmds);
        } else {
            regions.push((cur, j - 1));
        }
        out[cur + newlen..cur + newlen + (oe - os)].copy_from_slice(&d[os..oe]);
        cur += newlen + (oe - os);
    }

    out[cur..].fill(0); // padding (a reused buffer holds old bytes)

    // Block row ranges of about equal work, one per thread.
    let threads = crate::pool::threads_for(total);
    let work = |y: usize| pa[y] as usize + pt[y] as usize;
    let mut cuts = vec![0usize];
    for t in 1..threads {
        let goal = work(bh) * t / threads;
        let mut lo = cuts[cuts.len() - 1];
        let mut hi = bh;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if work(mid) < goal { lo = mid + 1 } else { hi = mid }
        }
        cuts.push(lo);
    }
    cuts.push(bh);

    // Cut each new stream's region at those rows and hand the pieces out.
    struct Job<'a> {
        parts: Vec<(&'a mut [u8], usize)>, // (output piece, stream)
        y0: usize,
        y1: usize,
    }
    let mut jobs: Vec<Job> = (0..threads).map(|t| Job { parts: Vec::with_capacity(regions.len()), y0: cuts[t], y1: cuts[t + 1] }).collect();
    let mut rest: &mut [u8] = &mut out[..];
    let mut base = 0usize;
    for &(start, s) in &regions {
        let (bytes, _, both) = st[s];
        let pre = if both { &pa } else { &pt };
        let (_, r) = std::mem::take(&mut rest).split_at_mut(start - base);
        let mut r = r;
        base = start;
        for job in jobs.iter_mut() {
            let len = 4 * bytes * (pre[job.y1] - pre[job.y0]) as usize;
            let (piece, r2) = r.split_at_mut(len);
            job.parts.push((piece, s));
            r = r2;
            base += len;
        }
        rest = r;
    }
    let (pa, pt) = (&pa, &pt);
    crate::pool::run_split(jobs, |job| {
        for (piece, s) in job.parts {
            let (bytes, kind, both) = st[s];
            let pre = if both { pa } else { pt };
            let os = h.offs[0][s + 1];
            let src = &d[os + pre[job.y0] as usize * bytes..os + pre[job.y1] as usize * bytes];
            double_stream(src, &pre[job.y0..=job.y1], bytes, kind, piece)?;
        }
        Some(())
    })?;
    Some(out)
}
