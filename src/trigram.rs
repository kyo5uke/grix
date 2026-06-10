//! Byte trigrams. A trigram is 3 consecutive bytes packed into a u32
//! (big-endian-ish: b0 is the high byte) so they sort in byte order.

pub const NUL_PROBE: usize = 8192;

#[inline]
pub fn pack(b0: u8, b1: u8, b2: u8) -> u32 {
    (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2)
}

#[inline]
pub fn pack_str(s: &[u8]) -> u32 {
    debug_assert!(s.len() == 3);
    pack(s[0], s[1], s[2])
}

/// Human-readable form for debugging ("abc" or hex escapes).
pub fn unpack_display(t: u32) -> String {
    let bytes = [(t >> 16) as u8, (t >> 8) as u8, t as u8];
    let mut out = String::new();
    for b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// True if the buffer smells like binary: a NUL byte in the first 8 KiB.
pub fn looks_binary(data: &[u8]) -> bool {
    let probe = &data[..data.len().min(NUL_PROBE)];
    probe.contains(&0)
}

/// Collect the distinct trigrams of `data`, sorted ascending.
pub fn extract_sorted(data: &[u8]) -> Vec<u32> {
    if data.len() < 3 {
        return Vec::new();
    }
    let mut tris: Vec<u32> = Vec::with_capacity(data.len().min(1 << 16));
    let mut t = (u32::from(data[0]) << 8) | u32::from(data[1]);
    for &b in &data[2..] {
        t = ((t << 8) | u32::from(b)) & 0x00ff_ffff;
        tris.push(t);
    }
    tris.sort_unstable();
    tris.dedup();
    tris
}

/// All trigrams of a (>=3 byte) string, in order of appearance (with duplicates).
pub fn of_str(s: &[u8]) -> impl Iterator<Item = u32> + '_ {
    s.windows(3).map(pack_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_basic() {
        let tris = extract_sorted(b"abcab");
        // windows: abc, bca, cab -> sorted: abc, bca, cab
        assert_eq!(
            tris,
            vec![pack_str(b"abc"), pack_str(b"bca"), pack_str(b"cab")]
        );
    }

    #[test]
    fn extract_dedups() {
        let tris = extract_sorted(b"aaaaaa");
        assert_eq!(tris, vec![pack_str(b"aaa")]);
    }

    #[test]
    fn short_input() {
        assert!(extract_sorted(b"").is_empty());
        assert!(extract_sorted(b"ab").is_empty());
    }

    #[test]
    fn binary_detect() {
        assert!(looks_binary(b"ab\0cd"));
        assert!(!looks_binary(b"plain text"));
    }

    #[test]
    fn display() {
        assert_eq!(unpack_display(pack_str(b"abc")), "abc");
        assert_eq!(unpack_display(pack(0, b'a', 0xff)), "\\x00a\\xff");
    }
}
