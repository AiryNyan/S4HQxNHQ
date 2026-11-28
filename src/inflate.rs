//! Minimal zlib/deflate decoder (no dependencies).

struct Br<'a> {
    d: &'a [u8],
    pos: usize,
    bit: u64,
    cnt: u32,
}

impl<'a> Br<'a> {
    #[inline]
    fn refill(&mut self) {
        // Whole 8-byte load while that much input is left, bytewise at the tail.
        if self.cnt <= 56 && self.pos + 8 <= self.d.len() {
            let p = self.pos;
            let w = u64::from_le_bytes([self.d[p], self.d[p + 1], self.d[p + 2], self.d[p + 3], self.d[p + 4], self.d[p + 5], self.d[p + 6], self.d[p + 7]]);
            self.bit |= w << self.cnt;
            let adv = (63 - self.cnt) / 8;
            self.pos += adv as usize;
            self.cnt += adv * 8;
            return;
        }
        while self.cnt <= 56 && self.pos < self.d.len() {
            self.bit |= (self.d[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }
    #[inline]
    fn bits(&mut self, n: u32) -> Option<u32> {
        if self.cnt < n {
            self.refill();
            if self.cnt < n {
                return None;
            }
        }
        let v = (self.bit & ((1u64 << n) - 1)) as u32;
        self.bit >>= n;
        self.cnt -= n;
        Some(v)
    }
    /// Drop the rest of the current byte and hand prefetched bytes back.
    fn align(&mut self) {
        self.cnt -= self.cnt % 8;
        self.pos -= (self.cnt / 8) as usize;
        self.bit = 0;
        self.cnt = 0;
    }
}

const FAST: u32 = 10;

struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
    /// Indexed by the next FAST input bits: symbol | length << 9, 0 = long code.
    fast: Vec<u16>,
}

fn build(lengths: &[u8]) -> Huff {
    let mut count = [0u16; 16];
    for &l in lengths {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut offs = [0u16; 16];
    for i in 1..16 {
        offs[i] = offs[i - 1] + count[i - 1];
    }
    let mut symbol = vec![0u16; lengths.len()];
    for (s, &l) in lengths.iter().enumerate() {
        if l != 0 {
            symbol[offs[l as usize] as usize] = s as u16;
            offs[l as usize] += 1;
        }
    }
    // Canonical codes, bit-reversed because deflate sends codes MSB first.
    let mut next = [0u32; 16];
    let mut code = 0u32;
    for len in 1..16 {
        next[len] = code;
        code = (code + count[len] as u32) << 1;
    }
    let mut fast = vec![0u16; 1 << FAST];
    for (s, &l) in lengths.iter().enumerate() {
        let l = l as u32;
        if l == 0 {
            continue;
        }
        let c = next[l as usize];
        next[l as usize] += 1;
        if l > FAST || c >= (1 << l) {
            continue;
        }
        let mut r = 0u32;
        for b in 0..l {
            r |= ((c >> b) & 1) << (l - 1 - b);
        }
        let mut i = r as usize;
        while i < (1 << FAST) {
            fast[i] = s as u16 | ((l as u16) << 9);
            i += 1 << l;
        }
    }
    Huff { count, symbol, fast }
}

#[inline]
fn decode(br: &mut Br, h: &Huff) -> Option<u16> {
    if br.cnt < 15 {
        br.refill();
    }
    let e = h.fast[(br.bit & ((1 << FAST) - 1)) as usize];
    if e != 0 {
        let l = (e >> 9) as u32;
        if l > br.cnt {
            return None;
        }
        br.bit >>= l;
        br.cnt -= l;
        return Some(e & 511);
    }
    let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
    for len in 1..16 {
        code |= br.bits(1)? as i32;
        let count = h.count[len] as i32;
        if code - count < first {
            return h.symbol.get((index + (code - first)) as usize).copied();
        }
        index += count;
        first += count;
        first <<= 1;
        code <<= 1;
    }
    None
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Inflate a zlib stream. Stops early (returning what it has) once `limit`
/// bytes are produced or the input runs out, so a truncated input can be
/// used to peek at the start of a resource.
pub fn inflate(src: &[u8], limit: usize) -> Option<Vec<u8>> {
    inflate_sized(src, limit, 0)
}

// libdeflate (third_party/libdeflate, MIT): whole-buffer decompression,
// linked statically by build.rs.
#[repr(C)]
struct Decompressor {
    _p: [u8; 0],
}
// The attribute (not build.rs) names the library, so every binary that
// compiles this module links it too.
#[link(name = "deflate", kind = "static")]
extern "C" {
    fn libdeflate_alloc_decompressor() -> *mut Decompressor;
    fn libdeflate_zlib_decompress(d: *mut Decompressor, src: *const u8, n: usize, out: *mut u8, avail: usize, actual: *mut usize) -> i32;
}

/// Off = always use the decoder below (comparison runs in hqxtest).
pub static USE_LIBDEFLATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

thread_local! {
    // One decompressor per thread (a decompressor must not be shared); kept
    // for the thread's lifetime.
    static DEC: *mut Decompressor = unsafe { libdeflate_alloc_decompressor() };
}

/// A whole stream that decompresses to at most `expect` bytes, or None (any
/// failure: the caller falls back to the decoder below).
fn whole(src: &[u8], expect: usize) -> Option<Vec<u8>> {
    let d = DEC.try_with(|d| *d).ok()?;
    if d.is_null() {
        return None;
    }
    // A kept buffer, or a new one left unwritten (libdeflate fills it; a
    // damaged size that cannot be allocated fails here instead of aborting).
    let mut out = match crate::pool::take_kept(expect) {
        Some(v) => v,
        None => {
            let mut v: Vec<u8> = Vec::new();
            v.try_reserve_exact(expect).ok()?;
            v
        }
    };
    let mut got = 0usize;
    let r = unsafe { libdeflate_zlib_decompress(d, src.as_ptr(), src.len(), out.as_mut_ptr(), expect, &mut got) };
    if r != 0 || got > expect {
        crate::pool::give(out);
        return None;
    }
    // SAFETY: libdeflate wrote `got` bytes at the start of the buffer, whose capacity is at least `expect`.
    unsafe { out.set_len(got) };
    Some(out)
}

/// libdeflate into a given buffer, which the stream must fill exactly (benchmark).
#[allow(dead_code)]
pub fn whole_into(src: &[u8], out: &mut [u8]) -> bool {
    let Ok(d) = DEC.try_with(|d| *d) else { return false };
    !d.is_null() && unsafe { libdeflate_zlib_decompress(d, src.as_ptr(), src.len(), out.as_mut_ptr(), out.len(), std::ptr::null_mut()) } == 0
}

/// `inflate` with the expected output size known in advance (0 = unknown).
pub fn inflate_sized(src: &[u8], limit: usize, expect: usize) -> Option<Vec<u8>> {
    // A full read (the limit allows more than the expected size): libdeflate,
    // whose result for a valid stream is the same bytes. A stream it rejects
    // (damaged, truncated, larger than expected) goes through the decoder
    // below, so such inputs behave exactly as before.
    if expect > 0 && limit > expect && USE_LIBDEFLATE.load(std::sync::atomic::Ordering::Relaxed) {
        if let Some(v) = whole(src, expect) {
            return Some(v);
        }
    }
    if src.len() < 2 || (src[0] & 0x0F) != 8 {
        return None;
    }
    let mut br = Br { d: src, pos: 2, bit: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(expect.min(64 << 20));
    let mut fixed: Option<(Huff, Huff)> = None;
    loop {
        let Some(last) = br.bits(1) else { return Some(out) };
        let Some(ty) = br.bits(2) else { return Some(out) };
        match ty {
            0 => {
                br.align();
                if br.pos + 4 > src.len() {
                    return Some(out);
                }
                let len = u16::from_le_bytes([src[br.pos], src[br.pos + 1]]) as usize;
                br.pos += 4;
                let end = (br.pos + len).min(src.len());
                out.extend_from_slice(&src[br.pos..end]);
                if end < br.pos + len {
                    return Some(out);
                }
                br.pos = end;
            }
            1 | 2 => {
                let dynamic;
                let (lh, dh) = if ty == 1 {
                    let f = fixed.get_or_insert_with(|| {
                        let mut l = [0u8; 288];
                        for (i, v) in l.iter_mut().enumerate() {
                            *v = if i < 144 { 8 } else if i < 256 { 9 } else if i < 280 { 7 } else { 8 };
                        }
                        (build(&l), build(&[5u8; 30]))
                    });
                    (&f.0, &f.1)
                } else {
                    let Some(nlen) = br.bits(5) else { return Some(out) };
                    let Some(ndist) = br.bits(5) else { return Some(out) };
                    let Some(ncode) = br.bits(4) else { return Some(out) };
                    let (nlen, ndist, ncode) = (nlen as usize + 257, ndist as usize + 1, ncode as usize + 4);
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &o in ORDER.iter().take(ncode) {
                        let Some(v) = br.bits(3) else { return Some(out) };
                        cl[o] = v as u8;
                    }
                    let ch = build(&cl);
                    let mut lens = vec![0u8; nlen + ndist];
                    let mut i = 0;
                    while i < nlen + ndist {
                        let Some(sym) = decode(&mut br, &ch) else { return Some(out) };
                        if sym < 16 {
                            lens[i] = sym as u8;
                            i += 1;
                        } else {
                            let (prev, rep) = match sym {
                                16 => {
                                    if i == 0 {
                                        return None;
                                    }
                                    let Some(r) = br.bits(2) else { return Some(out) };
                                    (lens[i - 1], 3 + r as usize)
                                }
                                17 => {
                                    let Some(r) = br.bits(3) else { return Some(out) };
                                    (0, 3 + r as usize)
                                }
                                18 => {
                                    let Some(r) = br.bits(7) else { return Some(out) };
                                    (0, 11 + r as usize)
                                }
                                _ => return None,
                            };
                            if i + rep > nlen + ndist {
                                return None;
                            }
                            for _ in 0..rep {
                                lens[i] = prev;
                                i += 1;
                            }
                        }
                    }
                    dynamic = (build(&lens[..nlen]), build(&lens[nlen..]));
                    (&dynamic.0, &dynamic.1)
                };
                loop {
                    let Some(sym) = decode(&mut br, lh) else { return Some(out) };
                    if sym < 256 {
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let s = sym as usize - 257;
                        if s >= 29 {
                            return None;
                        }
                        let Some(e) = br.bits(LEXT[s] as u32) else { return Some(out) };
                        let len = LBASE[s] as usize + e as usize;
                        let Some(ds) = decode(&mut br, dh) else { return Some(out) };
                        let ds = ds as usize;
                        if ds >= 30 {
                            return None;
                        }
                        let Some(e) = br.bits(DEXT[ds] as u32) else { return Some(out) };
                        let dist = DBASE[ds] as usize + e as usize;
                        if dist > out.len() {
                            return None;
                        }
                        let start = out.len() - dist;
                        if dist >= len {
                            out.extend_from_within(start..start + len);
                        } else if dist == 1 {
                            let b = out[start];
                            out.resize(out.len() + len, b);
                        } else {
                            // Overlapping copy: the output from `start` on repeats with
                            // period `dist`, so it can be extended by copying what is
                            // already there in growing pieces instead of byte by byte.
                            let end = out.len() + len;
                            while out.len() < end {
                                let n = (out.len() - start).min(end - out.len());
                                out.extend_from_within(start..start + n);
                            }
                        }
                    }
                    if out.len() >= limit {
                        return Some(out);
                    }
                }
            }
            _ => return None,
        }
        if last == 1 || out.len() >= limit {
            return Some(out);
        }
    }
}

/// zlib's checksum of the uncompressed data (verification only).
#[allow(dead_code)]
pub fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for c in d.chunks(5552) {
        for x in c {
            a += *x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}
