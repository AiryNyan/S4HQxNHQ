//! Little-endian field readers shared by the parsers.

pub fn u16at(d: &[u8], o: usize) -> usize {
    u16::from_le_bytes([d[o], d[o + 1]]) as usize
}

pub fn u32at(d: &[u8], o: usize) -> usize {
    u32le(d, o) as usize
}

pub fn u32le(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
