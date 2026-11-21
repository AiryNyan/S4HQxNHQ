// In-process end-to-end test: behaves like the game (kernel32 ReadFile etc. via the import table).
use std::convert::TryInto;
use std::ffi::c_void;
type P = *mut c_void;
#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: P, disp: u32, flags: u32, t: P) -> P;
    fn ReadFile(h: P, buf: *mut u8, n: u32, read: *mut u32, ovl: P) -> i32;
    fn SetFilePointerEx(h: P, dist: i64, newpos: *mut i64, method: u32) -> i32;
    fn GetFileSizeEx(h: P, out: *mut i64) -> i32;
    fn CloseHandle(h: P) -> i32;
    fn Sleep(ms: u32);
    fn LoadLibraryA(n: *const u8) -> P;
}
fn u32at(d: &[u8], o: usize) -> u32 { u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) }
unsafe fn rd(h: P, pos: u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    assert!(SetFilePointerEx(h, pos as i64, std::ptr::null_mut(), 0) != 0);
    let mut got = 0u32;
    assert!(ReadFile(h, v.as_mut_ptr(), n as u32, &mut got, std::ptr::null_mut()) != 0);
    v.truncate(got as usize);
    v
}
fn main() {
    unsafe {
        println!("dll {:p}", LoadLibraryA(b"d3d11.dll\0".as_ptr()));
        Sleep(300); // early hook thread
        let path = std::env::args().nth(1).unwrap();
        if path.to_lowercase().ends_with(".sgr") {
            let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let h = CreateFileW(w.as_ptr(), 0x8000_0000, 1, std::ptr::null_mut(), 3, 0x80, std::ptr::null_mut());
            let disk = std::fs::read(&path).unwrap();
            // read in odd-sized pieces, like a buffered reader would
            let mut seen = Vec::new();
            let mut pos = 0u64;
            loop {
                let d = rd(h, pos, 1000);
                if d.is_empty() { break; }
                pos += d.len() as u64;
                seen.extend_from_slice(&d);
            }
            CloseHandle(h);
            println!("sgr: disk {} bytes, seen {} bytes, differing bytes {}", disk.len(), seen.len(), disk.iter().zip(&seen).filter(|(a, b)| a != b).count());
            for l in String::from_utf8_lossy(&seen).lines().filter(|l| l.contains("SimTextureSize")) { println!("[{l}]"); }
            return;
        }
        let real = std::fs::metadata(&path).unwrap().len();
        let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let h = CreateFileW(w.as_ptr(), 0x8000_0000, 1, std::ptr::null_mut(), 3, 0x80, std::ptr::null_mut());
        assert!(h as isize != -1);
        let mut sz = 0i64;
        GetFileSizeEx(h, &mut sz);
        println!("real size {real}, reported size {sz}");
        let head = rd(h, 0, 4096);
        let n = u32at(&head, 36) as usize;
        let ipos = u64::from_le_bytes(head[64..72].try_into().unwrap());
        let isz = u32at(&head, 44) as usize;
        let idx = rd(h, ipos, isz);
        let flags = u32at(&idx, 0);
        let nconst = (0..4).filter(|b| flags & (1 << b) != 0).count();
        let esz = (4 - nconst) * 4 + 16;
        let base = 4 + nconst * 4;
        let (mut virt, mut checked) = (0, 0);
        for i in 0..n {
            let p = base + i * esz + (4 - nconst) * 4;
            let (off, fs, ms) = (u32at(&idx, p) as u64, u32at(&idx, p + 4) & 0x7FFF_FFFF, u32at(&idx, p + 8));
            let comp = u16::from_le_bytes([idx[p + 12], idx[p + 13]]);
            if off >= real {
                virt += 1;
                if virt % 400 == 1 || std::env::var_os("HQX_ALL").is_some() {
                    let d = rd(h, off, fs as usize);
                    assert_eq!(d.len(), fs as usize);
                    assert_eq!((fs, comp), (ms, 0));
                    let (tw, th, mips) = (u16::from_le_bytes([d[8], d[9]]), u16::from_le_bytes([d[10], d[11]]), u16::from_le_bytes([d[12], d[13]]));
                    println!("  virtual entry off={off} size={fs}: {} {} {tw}x{th} mips={mips}", String::from_utf8_lossy(&d[0..4]), String::from_utf8_lossy(&d[4..8]));
                    // sims portrait, pets and horses landscape
                    assert!([(2048, 4096), (1024, 2048), (4096, 2048), (2048, 1024)].contains(&(tw, th)), "unexpected size {tw}x{th}");
                    if let (Some(out), true) = (std::env::var_os("HQX_DUMP"), d.starts_with(b"DXT5RLES")) {
                        std::fs::write(out, &d).unwrap();
                    }
                    // the same entry in two reads, asking for the position in
                    // between (a zero move must not end the multi-call read)
                    let half = (fs as usize / 2) & !15;
                    let mut a = rd(h, off, half);
                    let mut cur = 0i64;
                    assert!(SetFilePointerEx(h, 0, &mut cur, 1) != 0);
                    assert_eq!(cur as u64, off + half as u64);
                    let mut b = vec![0u8; fs as usize - half];
                    let mut got = 0u32;
                    assert!(ReadFile(h, b.as_mut_ptr(), b.len() as u32, &mut got, std::ptr::null_mut()) != 0);
                    b.truncate(got as usize);
                    a.extend_from_slice(&b);
                    assert!(a == d, "split read with a position query differs");
                    // in chunks, with an absolute seek to each chunk (must not end the read)
                    let mut c = Vec::new();
                    let mut p = 0usize;
                    while p < fs as usize {
                        let k = 65536.min(fs as usize - p);
                        c.extend_from_slice(&rd(h, off + p as u64, k));
                        p += k;
                    }
                    assert!(c == d, "chunked read with absolute seeks differs (entry at {off})");
                    checked += 1;
                }
            }
        }
        println!("index entries {n}, virtual {virt}, read+checked {checked}");
        if std::env::var_os("HQX_BYPASS").is_some() {
            // what an unpatched index would say: read the raw index without the hooks
            use std::io::{Read as _, Seek as _, SeekFrom};
            let mut f = std::fs::File::open(&path).unwrap();
            let mut raw = vec![0u8; isz];
            f.seek(SeekFrom::Start(ipos)).unwrap();
            f.read_exact(&mut raw).unwrap();
            for i in 0..n {
                let p = base + i * esz + (4 - nconst) * 4;
                if (u32at(&idx, p) as u64) >= real {
                    let orig = u32at(&raw, p) as u64;
                    let d = rd(h, orig, 64);
                    println!("bypass: read original offset {orig} of a virtual entry, got {} bytes", d.len());
                    let d = rd(h, orig, 64);
                    println!("bypass: second read, got {} bytes (warning must appear once)", d.len());
                    break;
                }
            }
        }
        // an ordinary resource must be untouched: compare a real read with std::fs
        let plain = rd(h, 1_000_000, 65536);
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path).unwrap();
        let mut want = vec![0u8; 65536];
        f.seek(SeekFrom::Start(1_000_000)).unwrap();
        f.read_exact(&mut want).unwrap();
        println!("ordinary read identical: {}", plain == want);
        CloseHandle(h);
        if let Some(s) = std::env::var_os("HQX_WAIT") { Sleep(s.to_string_lossy().parse::<u32>().unwrap_or(0) * 1000); }
        if std::env::var_os("HQX_CRASH").is_some() { let p = 0x10 as *const u32; println!("{}", std::ptr::read_volatile(p)); }
    }
}
