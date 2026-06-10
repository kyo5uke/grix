//! LEB128 variable-length integers for posting lists.

/// Append `v` to `out` as LEB128.
pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Read one LEB128 value from `buf` starting at `pos`.
/// Returns (value, new_pos). None on truncated/overlong input.
pub fn read_u64(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(pos)?;
        pos += 1;
        if shift >= 64 {
            return None;
        }
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((v, pos));
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let vals = [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ];
        let mut buf = Vec::new();
        for &v in &vals {
            write_u64(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            let (got, np) = read_u64(&buf, pos).unwrap();
            assert_eq!(got, v);
            pos = np;
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn truncated() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 300);
        assert!(read_u64(&buf[..1], 0).is_none());
        assert!(read_u64(&[], 0).is_none());
    }
}
