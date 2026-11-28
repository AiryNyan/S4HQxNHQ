//! Sims 4 LRLE sim texture (skin colour, skin details, newer CAS parts):
//! parsing and an exact 2x pixel-doubling upscale done by re-encoding runs.
//!
//! Header: "LRLE", version (0 or "V002"), u16 w, u16 h, u32 mips, u32 offsets
//! per mip (relative to the mip data), V002 only: u32 palette count + 4-byte
//! palette entries. A mip is a command stream producing pixels 4x4 block by
//! block (blocks in raster order, 16 pixels per block in raster order).

/// Bytes of the resource needed by `parse_hdr`.
pub const PEEK: usize = 16 + 4 * 16 + 4;

/// Output of mip 0 is allowed to be this many times the source mip 0.
const GROW: usize = 12;

pub struct Hdr {
    pub v2: bool,
    pub w: usize,
    pub h: usize,
    pub mips: usize,
    pub offs: Vec<usize>,
    /// Start of the mip data (after offset table and palette).
    pub base: usize,
}

use crate::bytes::{u16at, u32at};

pub fn parse_hdr(d: &[u8]) -> Option<Hdr> {
    if d.len() < 16 || &d[0..4] != b"LRLE" {
        return None;
    }
    let v2 = match &d[4..8] {
        b"V002" => true,
        [0, 0, 0, 0] => false,
        _ => return None,
    };
    let (w, h, mips) = (u16at(d, 8), u16at(d, 10), u32at(d, 12));
    if mips == 0 || mips > 16 {
        return None;
    }
    let mut base = 16 + 4 * mips;
    if d.len() < base + if v2 { 4 } else { 0 } {
        return None;
    }
    let offs = (0..mips).map(|i| u32at(d, 16 + 4 * i)).collect();
    if v2 {
        let pal = u32at(d, base);
        if pal > 0x100_0000 {
            return None;
        }
        base += 4 + 4 * pal;
    }
    Some(Hdr { v2, w, h, mips, offs, base })
}

impl Hdr {
    /// Sims are portrait (1024x2048), pets and horses landscape (2048x1024).
    pub fn needs_upscale(&self) -> bool {
        ((self.w, self.h) == (1024, 2048) || (self.w, self.h) == (2048, 1024)) && self.mips >= 2
    }
    fn sane(&self, total: usize) -> bool {
        if self.w % 4 != 0 || self.h % 4 != 0 || self.mips < 2 || self.base > total || self.offs[0] != 0 {
            return false;
        }
        let len = total - self.base;
        self.offs.windows(2).all(|p| p[0] <= p[1]) && self.offs[self.mips - 1] <= len
    }
    /// Fixed size of `upscale()` output, computable from the header alone.
    pub fn bound(&self, total: usize) -> Option<usize> {
        if !self.sane(total) {
            return None;
        }
        // Measured on game data: growth is at most x7.2 for tiny levels and
        // x3.3 above 64 KB; a level that still does not fit is served transparent.
        let s0 = self.offs[1];
        let top = (GROW * s0).min(5 * s0 + (256 << 10));
        Some(self.base + top + 4 * self.h + 4096 + self.offs[self.mips - 1])
    }
}

const IDX: u64 = 1 << 32; // symbol flag: palette index (otherwise a raw 4-byte pixel)

/// Streaming decoder of one mip into symbols.
#[derive(Clone)]
struct Dec<'a> {
    d: &'a [u8],
    p: usize,
    end: usize,
    v2: bool,
    run: u64,
    run_left: usize,
    lit_left: usize,
    lit_raw: bool,
    planar: Vec<u32>,
    planar_pos: usize,
    err: bool,
}

impl<'a> Dec<'a> {
    fn new(d: &'a [u8], start: usize, end: usize, v2: bool) -> Self {
        Dec { d, p: start, end, v2, run: 0, run_left: 0, lit_left: 0, lit_raw: true, planar: Vec::new(), planar_pos: 0, err: false }
    }
    fn byte(&mut self) -> Option<u8> {
        if self.p < self.end {
            self.p += 1;
            Some(self.d[self.p - 1])
        } else {
            self.err = true;
            None
        }
    }
    fn raw(&mut self) -> Option<u64> {
        if self.p + 4 > self.end {
            self.err = true;
            return None;
        }
        self.p += 4;
        Some(u32at(self.d, self.p - 4) as u64)
    }
    /// Continue a count whose first byte was `b` and which holds `n` so far.
    fn cont(&mut self, mut b: u8, mut n: usize, mut shift: u32) -> Option<usize> {
        while b & 0x80 != 0 {
            b = self.byte()?;
            if shift > 40 {
                self.err = true;
                return None;
            }
            n += ((b & 0x7F) as usize) << shift;
            shift += 7;
        }
        Some(n)
    }
    fn planar(&mut self, n: usize) -> Option<()> {
        let mut t: Vec<u8> = Vec::with_capacity(4 * n);
        while t.len() < 4 * n {
            let b = self.byte()?;
            if b & 1 != 0 {
                let mut k = ((b & 0x7F) >> 1) as usize;
                if b & 0x80 != 0 {
                    k += (self.byte()? as usize) << 6;
                }
                if self.p + k > self.end {
                    self.err = true;
                    return None;
                }
                t.extend_from_slice(&self.d[self.p..self.p + k]);
                self.p += k;
            } else {
                let mut k = ((b & 0x7F) >> 2) as usize;
                if b & 0x80 != 0 {
                    k += (self.byte()? as usize) << 5;
                }
                let v = if b & 2 != 0 { self.byte()? } else { 0 };
                t.resize(t.len() + k, v);
            }
        }
        if t.len() != 4 * n {
            self.err = true;
            return None;
        }
        self.planar.clear();
        self.planar_pos = 0;
        for i in 0..n {
            self.planar.push(u32::from_le_bytes([t[i], t[i + n], t[i + 2 * n], t[i + 3 * n]]));
        }
        Some(())
    }
    /// Fill `buf` completely; None at the end of the data or on an error.
    fn fill(&mut self, buf: &mut [u64]) -> Option<()> {
        let mut i = 0;
        while i < buf.len() {
            if self.run_left > 0 {
                let n = self.run_left.min(buf.len() - i);
                buf[i..i + n].fill(self.run);
                self.run_left -= n;
                i += n;
            } else if self.lit_left > 0 && self.lit_raw {
                // literal pixels stored as 4 raw bytes each
                let n = self.lit_left.min(buf.len() - i);
                if self.p + 4 * n > self.end {
                    self.err = true;
                    return None;
                }
                for (o, c) in buf[i..i + n].iter_mut().zip(self.d[self.p..self.p + 4 * n].chunks_exact(4)) {
                    *o = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64;
                }
                self.p += 4 * n;
                self.lit_left -= n;
                i += n;
            } else if self.lit_left > 0 {
                // literal palette indices, one byte each unless the high bit continues
                while i < buf.len() && self.lit_left > 0 {
                    let b = self.byte()?;
                    let v = if b < 0x80 { b as usize } else { self.cont(b, (b & 0x7F) as usize, 7)? };
                    buf[i] = IDX | v as u64;
                    i += 1;
                    self.lit_left -= 1;
                }
            } else {
                buf[i] = self.next_or_pad()?;
                i += 1;
            }
        }
        Some(())
    }
    /// Like `next`, but a level whose data ends cleanly before all its pixels
    /// (seen in CC) continues with transparent pixels.
    fn next_or_pad(&mut self) -> Option<u64> {
        match self.next() {
            None if !self.err => Some(0),
            r => r,
        }
    }
    fn next(&mut self) -> Option<u64> {
        loop {
            if self.run_left > 0 {
                self.run_left -= 1;
                return Some(self.run);
            }
            if self.lit_left > 0 {
                self.lit_left -= 1;
                if self.lit_raw {
                    return self.raw();
                }
                let b = self.byte()?;
                let n = self.cont(b, (b & 0x7F) as usize, 7)?;
                return Some(IDX | n as u64);
            }
            if self.planar_pos < self.planar.len() {
                self.planar_pos += 1;
                return Some(self.planar[self.planar_pos - 1] as u64);
            }
            if self.p >= self.end {
                return None;
            }
            self.command()?;
        }
    }
    /// Read one command and set up the run, literals or planar block it starts.
    fn command(&mut self) -> Option<()> {
        let b = self.byte()?;
        if self.v2 {
            if b & 1 != 0 {
                self.lit_left = self.cont(b, ((b >> 2) & 0x1F) as usize, 5)?;
                self.lit_raw = b & 2 != 0;
            } else {
                let n = self.cont(b, ((b >> 3) & 0x0F) as usize, 4)?;
                self.run = match b & 7 {
                    2 => IDX | self.byte()? as u64,
                    4 => IDX | (self.byte()? as u64) | ((self.byte()? as u64) << 8),
                    6 => self.raw()?,
                    _ => {
                        self.err = true;
                        return None;
                    }
                };
                self.run_left = n;
            }
        } else {
            match b & 3 {
                1 => {
                    self.lit_left = (b >> 2) as usize;
                    self.lit_raw = true;
                }
                3 => self.planar((b >> 2) as usize)?,
                op => {
                    let n = self.cont(b, ((b >> 2) & 0x1F) as usize, 5)?;
                    self.run = if op == 2 { self.raw()? } else { 0 };
                    self.run_left = n;
                }
            }
        }
        Some(())
    }
    /// Pass over `n` symbols without producing them (only commands and the
    /// bytes of index literals are read). A level whose data ends early
    /// continues with padding, as in `next_or_pad`.
    fn skip(&mut self, mut n: usize) -> Option<()> {
        while n > 0 {
            if self.run_left > 0 {
                let k = self.run_left.min(n);
                self.run_left -= k;
                n -= k;
            } else if self.lit_left > 0 {
                let k = self.lit_left.min(n);
                if self.lit_raw {
                    if self.p + 4 * k > self.end {
                        self.err = true;
                        return None;
                    }
                    self.p += 4 * k;
                } else {
                    for _ in 0..k {
                        let b = self.byte()?;
                        if b >= 0x80 {
                            self.cont(b, 0, 7)?;
                        }
                    }
                }
                self.lit_left -= k;
                n -= k;
            } else if self.planar_pos < self.planar.len() {
                let k = (self.planar.len() - self.planar_pos).min(n);
                self.planar_pos += k;
                n -= k;
            } else if self.p >= self.end {
                return Some(()); // the rest is padding
            } else {
                self.command()?;
            }
        }
        Some(())
    }
}

/// Greedy run encoder: runs of 2+ equal symbols become repeats, the rest literals.
struct Enc {
    v2: bool,
    out: Vec<u8>,
    lit: Vec<u64>,
    cur: u64,
    len: usize,
}

impl Enc {
    #[inline]
    fn count(&mut self, op: u8, bits: u32, n: usize) {
        self.cmd(op, bits, n, &[]);
    }
    /// Append one command: opcode with a variable-length count, then `tail`
    /// (at most 4 bytes), in a single buffer append.
    #[inline]
    fn cmd(&mut self, op: u8, bits: u32, mut n: usize, tail: &[u8]) {
        let mask = (1usize << bits) - 1;
        let mut buf = [0u8; 16];
        let mut k = 0;
        let mut b = op | (((n & mask) as u8) << (7 - bits));
        n >>= bits;
        while n > 0 {
            buf[k] = b | 0x80;
            k += 1;
            b = (n & 0x7F) as u8;
            n >>= 7;
        }
        buf[k] = b;
        k += 1;
        buf[k..k + tail.len()].copy_from_slice(tail);
        self.out.extend_from_slice(&buf[..k + tail.len()]);
    }
    #[inline]
    fn repeat(&mut self, s: u64, n: usize) {
        if !self.v2 {
            if s == 0 {
                self.cmd(0, 5, n, &[]);
            } else {
                self.cmd(2, 5, n, &(s as u32).to_le_bytes());
            }
        } else if s & IDX == 0 {
            self.cmd(6, 4, n, &(s as u32).to_le_bytes());
        } else if s & 0xFFFF_FF00 == 0 {
            self.cmd(2, 4, n, &[s as u8]);
        } else {
            self.cmd(4, 4, n, &(s as u16).to_le_bytes());
        }
    }
    fn flush_lit(&mut self) {
        let lit = std::mem::take(&mut self.lit);
        if lit.is_empty() {
            return;
        }
        if !self.v2 {
            for c in lit.chunks(63) {
                self.out.push(((c.len() as u8) << 2) | 1);
                for s in c {
                    self.out.extend_from_slice(&(*s as u32).to_le_bytes());
                }
            }
        } else if lit[0] & IDX == 0 {
            self.count(3, 5, lit.len());
            for s in &lit {
                self.out.extend_from_slice(&(*s as u32).to_le_bytes());
            }
        } else {
            self.count(1, 5, lit.len());
            for s in &lit {
                let mut n = (*s as u32) as usize;
                while n > 0x7F {
                    self.out.push((n & 0x7F) as u8 | 0x80);
                    n >>= 7;
                }
                self.out.push(n as u8);
            }
        }
        self.lit = lit;
        self.lit.clear();
    }
    #[inline]
    fn end_run(&mut self) {
        if self.len == 0 {
            return;
        }
        let (s, n) = (self.cur, self.len);
        self.len = 0;
        // Hot case in detailed textures: a short run of one palette colour.
        // Written without a data-dependent branch (index below or above 256).
        if self.v2 && (2..16).contains(&n) && s & IDX != 0 && s & 0xFFFF_0000 == 0 && self.lit.is_empty() {
            let idx = s as u32;
            let wide = (idx >= 256) as usize;
            self.out.reserve(3);
            let l = self.out.len();
            // SAFETY: 3 bytes were reserved; 2 or 3 of them become part of the vector.
            unsafe {
                let p = self.out.as_mut_ptr().add(l);
                *p = (2 + 2 * wide as u8) | ((n as u8) << 3);
                *p.add(1) = idx as u8;
                *p.add(2) = (idx >> 8) as u8;
                self.out.set_len(l + 2 + wide);
            }
            return;
        }
        let rep =if self.v2 { n >= 2 && (s & IDX == 0 || s & 0xFFFF_0000 == 0) } else { s == 0 || n >= 2 };
        if rep {
            if !self.lit.is_empty() {
                self.flush_lit();
            }
            self.repeat(s, n);
        } else {
            if self.v2 && self.lit.first().is_some_and(|f| (f ^ s) & IDX != 0) {
                self.flush_lit();
            }
            self.lit.resize(self.lit.len() + n, s);
        }
    }
    #[inline]
    fn push_n(&mut self, s: u64, n: usize) {
        if self.len > 0 && s == self.cur {
            self.len += n;
        } else {
            self.end_run();
            self.cur = s;
            self.len = n;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        self.end_run();
        self.flush_lit();
        self.out
    }
}

/// Source block rows [y0, y1) of the top level, decoded from `dec` and
/// encoded doubled into `enc` (each source block row becomes two output rows).
fn encode_rows(dec: &mut Dec, enc: &mut Enc, bw: usize, y0: usize, y1: usize) -> Option<()> {
    let row_px = bw * 16;
    let mut row = vec![0u64; row_px];
    let mut uniform = vec![false; bw];
    let mut y = y0;
    while y < y1 {
        // One source block row becomes two output block rows (64 * bw pixels).
        let first = dec.next_or_pad()?;
        // Fast path: a repeat run covering whole block rows (large flat or
        // transparent areas) is passed on as one run. `run_left` > 0 means
        // `first` came from that run.
        let whole = ((dec.run_left + 1) / row_px).min(y1 - y);
        if whole > 0 {
            dec.run_left -= whole * row_px - 1;
            enc.push_n(first, 64 * bw * whole);
            y += whole;
            continue;
        }
        row[0] = first;
        dec.fill(&mut row[1..])?;
        for (u, blk) in uniform.iter_mut().zip(row.chunks_exact(16)) {
            *u = blk[1..].iter().all(|s| *s == blk[0]);
        }
        for qy in 0..2 {
            for (u, blk) in uniform.iter().zip(row.chunks_exact(16)) {
                if *u {
                    enc.push_n(blk[0], 32);
                    continue;
                }
                let top = &blk[qy * 8..qy * 8 + 8];
                for qx in 0..2 {
                    // each source row of the quadrant is emitted twice, each pixel doubled
                    for r in 0..2 {
                        let (a, b) = (top[r * 4 + qx * 2], top[r * 4 + qx * 2 + 1]);
                        if a == b {
                            enc.push_n(a, 8);
                        } else {
                            enc.push_n(a, 2);
                            enc.push_n(b, 2);
                            enc.push_n(a, 2);
                            enc.push_n(b, 2);
                        }
                    }
                }
            }
        }
        y += 1;
    }
    Some(())
}

/// A large top level (compressed size from `SPLIT_MIN_BYTES`) is encoded
/// in this many independent pieces (block row ranges), which can run on
/// several threads. Fixed, so the output does not depend on how many
/// threads were free.
const PIECES: usize = 8;

/// Pieces whose start in the data is known, handed out to the threads.
struct Ready<'a> {
    queue: std::collections::VecDeque<(usize, Dec<'a>)>,
    scanned: bool,
    failed: bool,
    end: Option<Dec<'a>>,
}

/// Exact 2x upscale. The mip count is kept: a new top level is generated and
/// the smallest original level is dropped. The result is padded with no-op
/// commands to `size` (default `bound()`). When the encoded top level does
/// not fit it is replaced by a fully transparent one and `.1` holds the size
/// that would have been needed.
pub fn upscale(d: &[u8], size: Option<usize>) -> Option<(Vec<u8>, Option<usize>)> {
    let h = parse_hdr(d)?;
    let total = d.len();
    let bound = size.unwrap_or(h.bound(total)?);
    if !h.sane(total) || bound < h.base + h.offs[h.mips - 1] + 64 {
        return None;
    }
    let (bw, bh) = (h.w / 4, h.h / 4);
    if bw == 0 || bh == 0 {
        return None;
    }
    let pieces = if h.offs[1] >= crate::pool::SPLIT_MIN_BYTES && bh >= PIECES { PIECES } else { 1 };
    let cut = |i: usize| bh * i / pieces;
    let threads = if pieces > 1 { crate::pool::threads_for(h.offs[1]) } else { 1 };
    let new_enc = |out: Vec<u8>| Enc { v2: h.v2, out, lit: Vec::new(), cur: 0, len: 0 };
    // The output buffer, with room for the header in front: the first piece
    // (on one thread every piece, one after another) is encoded straight
    // into it, so only the other pieces are copied.
    let mut out = crate::pool::take_empty(bound);
    out.resize(h.base, 0);
    let mut dec = Dec::new(d, h.base, h.base + h.offs[1], h.v2);
    let mut encs: Vec<Enc> = Vec::with_capacity(pieces);
    if threads <= 1 {
        for i in 0..pieces {
            let mut enc = new_enc(out);
            let r = encode_rows(&mut dec, &mut enc, bw, cut(i), cut(i + 1));
            out = enc.finish();
            if r.is_none() {
                crate::pool::give(out);
                return None;
            }
        }
    } else {
        encs.push(new_enc(std::mem::take(&mut out)));
        for _ in 1..pieces {
            encs.push(new_enc(crate::pool::take_empty((2 * h.offs[1] + 1024) / pieces)));
        }
        // One thread passes over the data to find where each piece starts
        // (much faster than decoding it) and hands each piece out as soon as
        // its start is known; all threads, that one too once done, decode
        // and encode the pieces.
        let slots: Vec<std::sync::Mutex<&mut Enc>> = encs.iter_mut().map(std::sync::Mutex::new).collect();
        let ready = std::sync::Mutex::new(Ready { queue: Default::default(), scanned: false, failed: false, end: None });
        let cv = std::sync::Condvar::new();
        let lock = || ready.lock().unwrap_or_else(|p| p.into_inner());
        let start = dec.clone();
        crate::pool::run_split((0..threads).collect(), |t| {
            if t == 0 {
                // marks the scan done however it ends (a panic too), so no thread waits forever
                struct Done<'r, 'a>(&'r std::sync::Mutex<Ready<'a>>, &'r std::sync::Condvar);
                impl Drop for Done<'_, '_> {
                    fn drop(&mut self) {
                        let mut r = self.0.lock().unwrap_or_else(|p| p.into_inner());
                        if !r.scanned {
                            r.scanned = true;
                            r.failed = true; // not reached the end normally
                        }
                        drop(r);
                        self.1.notify_all();
                    }
                }
                let _done = Done(&ready, &cv);
                let mut sc = start.clone();
                for i in 0..pieces {
                    lock().queue.push_back((i, sc.clone()));
                    cv.notify_one();
                    if i + 1 < pieces && sc.skip((cut(i + 1) - cut(i)) * bw * 16).is_none() {
                        lock().failed = true;
                        break;
                    }
                }
                let mut r = lock();
                r.scanned = true;
                drop(r);
                cv.notify_all();
            }
            loop {
                let (i, mut dc) = {
                    let mut r = lock();
                    loop {
                        if let Some(p) = r.queue.pop_front() {
                            break p;
                        }
                        if r.scanned {
                            return Some(());
                        }
                        r = cv.wait(r).unwrap_or_else(|p| p.into_inner());
                    }
                };
                let mut enc = slots[i].lock().unwrap_or_else(|p| p.into_inner());
                if encode_rows(&mut dc, &mut enc, bw, cut(i), cut(i + 1)).is_none() {
                    lock().failed = true;
                } else if i + 1 == pieces {
                    lock().end = Some(dc);
                }
            }
        })?;
        let r = ready.into_inner().unwrap_or_else(|p| p.into_inner());
        if r.failed {
            return None;
        }
        dec = r.end?;
    }
    let keep = h.offs[h.mips - 1];
    let mut encs = encs.into_iter().map(Enc::finish);
    if let Some(first) = encs.next() {
        out = first;
    }
    for t in encs {
        out.extend_from_slice(&t);
        crate::pool::give(t);
    }
    if dec.next().is_some() || dec.err {
        crate::pool::give(out);
        return None;
    }
    let mut top_len = out.len() - h.base;
    let needed = h.base + top_len + keep;
    let fits = needed <= bound;
    if !fits {
        out.truncate(h.base);
        let mut e = Enc { v2: h.v2, out, lit: Vec::new(), cur: 0, len: 0 };
        e.repeat(0, h.w * h.h * 4);
        out = e.out;
        top_len = out.len() - h.base;
    }
    // header: the same layout as the source, offsets moved by the new top level
    let mut hd: Vec<u8> = Vec::with_capacity(h.base);
    hd.extend_from_slice(&d[0..8]);
    hd.extend_from_slice(&((h.w * 2) as u16).to_le_bytes());
    hd.extend_from_slice(&((h.h * 2) as u16).to_le_bytes());
    hd.extend_from_slice(&(h.mips as u32).to_le_bytes());
    hd.extend_from_slice(&0u32.to_le_bytes());
    for i in 0..h.mips - 1 {
        hd.extend_from_slice(&((top_len + h.offs[i]) as u32).to_le_bytes());
    }
    hd.extend_from_slice(&d[16 + 4 * h.mips..h.base]);
    if hd.len() != h.base {
        crate::pool::give(out);
        return None;
    }
    out[..h.base].copy_from_slice(&hd);
    out.extend_from_slice(&d[h.base..h.base + keep]);
    // Padding lands in the last mip: use zero-length literal runs, which decode to nothing.
    out.resize(bound, if h.v2 { 3 } else { 1 });
    Some((out, (!fits).then_some(needed)))
}

/// A valid, fully transparent LRLE (version 0) of the given size and mip
/// count: one transparent run per level. Served in place of an upscale that
/// failed. Padded with zero-length literals to `size`.
pub fn transparent(w: usize, h: usize, mips: usize, size: usize) -> Option<Vec<u8>> {
    if mips == 0 || mips > 16 || w == 0 || h == 0 {
        return None;
    }
    let mut body: Vec<u8> = Vec::new();
    let mut offs = Vec::with_capacity(mips);
    for i in 0..mips {
        offs.push(body.len());
        let mut e = Enc { v2: false, out: Vec::new(), lit: Vec::new(), cur: 0, len: 0 };
        e.repeat(0, (w >> i).max(1) * (h >> i).max(1));
        body.extend_from_slice(&e.out);
    }
    let base = 16 + 4 * mips;
    if base + body.len() > size {
        return None;
    }
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"LRLE\0\0\0\0");
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.extend_from_slice(&(mips as u32).to_le_bytes());
    for o in offs {
        out.extend_from_slice(&(o as u32).to_le_bytes());
    }
    out.extend_from_slice(&body);
    out.resize(size, 1);
    Some(out)
}

/// Decode one mip to pixels in raster order (verification only).
#[allow(dead_code)]
pub fn decode(d: &[u8], mip: usize) -> Option<(usize, usize, Vec<u32>)> {
    let h = parse_hdr(d)?;
    let (w, hh) = ((h.w >> mip).max(1), (h.h >> mip).max(1));
    let end = if mip + 1 < h.mips { h.base + h.offs[mip + 1] } else { d.len() };
    let mut dec = Dec::new(d, h.base + h.offs[mip], end, h.v2);
    let pal = 16 + 4 * h.mips + 4;
    let mut px = vec![0u32; w * hh];
    let bw = w / 4;
    for i in 0..w * hh {
        let s = dec.next_or_pad()?;
        let c = if s & IDX != 0 {
            let o = pal + 4 * (s as u32) as usize;
            if o + 4 > h.base {
                return None;
            }
            u32at(d, o) as u32
        } else {
            s as u32
        };
        let (b, k) = (i / 16, i % 16);
        px[((b / bw) * 4 + k / 4) * w + (b % bw) * 4 + k % 4] = c;
    }
    if dec.next().is_some() || dec.err {
        return None;
    }
    Some((w, hh, px))
}

/// Benchmark: pass over the whole top level without decoding (cost of finding piece starts).
#[allow(dead_code)]
pub fn scan_only(d: &[u8]) -> Option<()> {
    let h = parse_hdr(d)?;
    let mut dec = Dec::new(d, h.base, h.base + h.offs[1], h.v2);
    dec.skip(h.w * h.h)
}
