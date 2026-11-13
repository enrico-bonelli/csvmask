//! Byte search primitives used by the CSV scanner.
//!
//! On x86_64 they use SSE2 (always available on that architecture, so no runtime
//! detection is needed and the functions stay inlinable). Elsewhere they fall back
//! to a portable SWAR implementation (8 bytes per step).

#[cfg(target_arch = "x86_64")]
pub use sse2::{find_byte, find_byte2, rfind_byte};

#[cfg(not(target_arch = "x86_64"))]
pub use portable::{find_byte, find_byte2, rfind_byte};

/// Which structural bytes a `Cursor` search should stop at.
#[derive(Clone, Copy)]
pub enum Want {
    DelimOrNewline,
    Quote,
    QuoteOrNewline,
}

/// Forward scanner over the structural bytes (`delim`, `\n`, `"`) of a buffer.
///
/// Bitmasks are computed 64 bytes at a time, independently of the scan position, so
/// jumping from one structural byte to the next is a couple of bit operations instead
/// of a fresh vector search per field (which would be bound by load→compare→tzcnt latency).
pub struct Cursor<'a> {
    data: &'a [u8],
    delim: u8,
    base: usize,
    m_delim: u64,
    m_nl: u64,
    m_quote: u64,
}

impl<'a> Cursor<'a> {
    #[inline]
    pub fn new(data: &'a [u8], delim: u8) -> Self {
        Self {
            data,
            delim,
            base: usize::MAX,
            m_delim: 0,
            m_nl: 0,
            m_quote: 0,
        }
    }

    #[inline(always)]
    fn load(&mut self, base: usize) {
        let rest = &self.data[base..];
        let (d, nl, q) = if rest.len() >= 64 {
            masks64(rest[..64].try_into().unwrap(), self.delim)
        } else {
            let mut block = [0u8; 64];
            block[..rest.len()].copy_from_slice(rest);
            let (d, nl, q) = masks64(&block, self.delim);
            let valid = (1u64 << rest.len()) - 1;
            (d & valid, nl & valid, q & valid)
        };
        self.base = base;
        self.m_delim = d;
        self.m_nl = nl;
        self.m_quote = q;
    }

    #[inline(always)]
    fn select(&self, want: Want) -> u64 {
        match want {
            Want::DelimOrNewline => self.m_delim | self.m_nl,
            Want::Quote => self.m_quote,
            Want::QuoteOrNewline => self.m_quote | self.m_nl,
        }
    }

    /// Index of the first wanted structural byte at or after `pos`.
    #[inline(always)]
    pub fn next(&mut self, pos: usize, want: Want) -> Option<usize> {
        if pos >= self.data.len() {
            return None;
        }
        let mut base = pos & !63;
        if base != self.base {
            self.load(base);
        }
        let mut m = self.select(want) & (u64::MAX << (pos - base));
        loop {
            if m != 0 {
                return Some(base + m.trailing_zeros() as usize);
            }
            base += 64;
            if base >= self.data.len() {
                return None;
            }
            self.load(base);
            m = self.select(want);
        }
    }
}

/// Bitmasks of `delim`, `\n` and `"` bytes in a 64-byte block (bit `i` = byte `i`).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn masks64(block: &[u8; 64], delim: u8) -> (u64, u64, u64) {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };
    // SAFETY: SSE2 is part of the x86_64 baseline; each load reads 16 bytes inside `block`.
    unsafe {
        let vd = _mm_set1_epi8(delim as i8);
        let vn = _mm_set1_epi8(b'\n' as i8);
        let vq = _mm_set1_epi8(b'"' as i8);
        let (mut d, mut nl, mut q) = (0u64, 0u64, 0u64);
        for k in 0..4 {
            let x = _mm_loadu_si128(block.as_ptr().add(16 * k).cast::<__m128i>());
            let sh = 16 * k as u32;
            d |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(x, vd)) as u32 & 0xFFFF) as u64) << sh;
            nl |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(x, vn)) as u32 & 0xFFFF) as u64) << sh;
            q |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(x, vq)) as u32 & 0xFFFF) as u64) << sh;
        }
        (d, nl, q)
    }
}

/// Bitmasks of `delim`, `\n` and `"` bytes in a 64-byte block (bit `i` = byte `i`).
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn masks64(block: &[u8; 64], delim: u8) -> (u64, u64, u64) {
    let (mut d, mut nl, mut q) = (0u64, 0u64, 0u64);
    for (i, &b) in block.iter().enumerate() {
        d |= u64::from(b == delim) << i;
        nl |= u64::from(b == b'\n') << i;
        q |= u64::from(b == b'"') << i;
    }
    (d, nl, q)
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128, _mm_set1_epi8,
    };

    const W: usize = 16;

    /// Bitmask (bit `i` = byte `i`) of the bytes at `p..p+16` equal to any needle.
    ///
    /// # Safety
    /// `p..p+16` must be readable.
    #[inline(always)]
    unsafe fn hits<const N: usize>(p: *const u8, needles: &[__m128i; N]) -> u32 {
        // SAFETY: the caller guarantees 16 readable bytes (unaligned loads are allowed);
        // SSE2 is part of the x86_64 baseline.
        unsafe {
            let x = _mm_loadu_si128(p.cast::<__m128i>());
            let mut acc = _mm_cmpeq_epi8(x, needles[0]);
            for v in &needles[1..] {
                acc = _mm_or_si128(acc, _mm_cmpeq_epi8(x, *v));
            }
            _mm_movemask_epi8(acc) as u32
        }
    }

    #[inline(always)]
    fn find_any<const N: usize>(h: &[u8], bytes: [u8; N]) -> Option<usize> {
        let n = h.len();
        if n < W {
            return h.iter().position(|b| bytes.contains(b));
        }
        // SAFETY: SSE2 is part of the x86_64 baseline.
        let needles = bytes.map(|b| unsafe { _mm_set1_epi8(b as i8) });
        let p = h.as_ptr();
        // SAFETY: every load below reads 16 bytes starting at an offset `o` with
        // `o + 16 <= n`, i.e. fully inside `h`.
        unsafe {
            // Most searches end within the first few bytes (short CSV fields).
            let m = hits(p, &needles);
            if m != 0 {
                return Some(m.trailing_zeros() as usize);
            }
            let mut i = W;
            while i + 2 * W <= n {
                let a = hits(p.add(i), &needles);
                let b = hits(p.add(i + W), &needles);
                if (a | b) != 0 {
                    return Some(if a != 0 {
                        i + a.trailing_zeros() as usize
                    } else {
                        i + W + b.trailing_zeros() as usize
                    });
                }
                i += 2 * W;
            }
            if i + W <= n {
                let m = hits(p.add(i), &needles);
                if m != 0 {
                    return Some(i + m.trailing_zeros() as usize);
                }
                i += W;
            }
            if i < n {
                // Overlapping final load; discard the bytes already checked.
                let base = n - W;
                let m = hits(p.add(base), &needles) >> (i - base);
                if m != 0 {
                    return Some(i + m.trailing_zeros() as usize);
                }
            }
        }
        None
    }

    /// Find the index of the first occurrence of `b1` in `haystack`.
    #[inline]
    pub fn find_byte(haystack: &[u8], b1: u8) -> Option<usize> {
        find_any(haystack, [b1])
    }

    /// Find the index of the first occurrence of `b1` or `b2` in `haystack`.
    #[inline]
    pub fn find_byte2(haystack: &[u8], b1: u8, b2: u8) -> Option<usize> {
        find_any(haystack, [b1, b2])
    }

    /// Find the index of the last occurrence of `b1` in `haystack`.
    #[inline]
    pub fn rfind_byte(haystack: &[u8], b1: u8) -> Option<usize> {
        let n = haystack.len();
        if n < W {
            return haystack.iter().rposition(|&b| b == b1);
        }
        // SAFETY: SSE2 is part of the x86_64 baseline.
        let needles = [unsafe { _mm_set1_epi8(b1 as i8) }];
        let p = haystack.as_ptr();
        // SAFETY: every load reads 16 bytes at an offset `o` with `o + 16 <= n`.
        unsafe {
            let mut end = n; // bytes at `end..` have been checked
            while end >= W {
                let base = end - W;
                let m = hits(p.add(base), &needles);
                if m != 0 {
                    return Some(base + 31 - m.leading_zeros() as usize);
                }
                end = base;
            }
            if end > 0 {
                // Overlapping first load; keep only the bytes before `end`.
                let m = hits(p, &needles) & ((1u32 << end) - 1);
                if m != 0 {
                    return Some(31 - m.leading_zeros() as usize);
                }
            }
        }
        None
    }
}

#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
mod portable {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;

    #[inline(always)]
    const fn splat(b: u8) -> u64 {
        (b as u64) * LO
    }

    #[inline(always)]
    const fn eq_mask(w: u64, target_splat: u64) -> u64 {
        let m = w ^ target_splat;
        m.wrapping_sub(LO) & !m
    }

    #[inline(always)]
    fn load(chunk: &[u8]) -> u64 {
        u64::from_le_bytes(chunk.try_into().unwrap())
    }

    #[inline(always)]
    fn load8(chunk: &[u8; 8]) -> u64 {
        u64::from_le_bytes(*chunk)
    }

    /// Find the index of the first occurrence of `b1` in `haystack`.
    #[inline]
    pub fn find_byte(haystack: &[u8], b1: u8) -> Option<usize> {
        let s1 = splat(b1);
        let mut offset = 0;
        let (chunks, rest) = haystack.as_chunks::<8>();
        for chunk in chunks {
            let hit = eq_mask(load8(chunk), s1) & HI;
            if hit != 0 {
                return Some(offset + (hit.trailing_zeros() as usize >> 3));
            }
            offset += 8;
        }
        rest.iter().position(|&b| b == b1).map(|i| offset + i)
    }

    /// Find the index of the first occurrence of `b1` or `b2` in `haystack`.
    #[inline]
    pub fn find_byte2(haystack: &[u8], b1: u8, b2: u8) -> Option<usize> {
        let s1 = splat(b1);
        let s2 = splat(b2);
        let mut offset = 0;
        let (chunks, rest) = haystack.as_chunks::<8>();
        for chunk in chunks {
            let w = load8(chunk);
            let hit = (eq_mask(w, s1) | eq_mask(w, s2)) & HI;
            if hit != 0 {
                return Some(offset + (hit.trailing_zeros() as usize >> 3));
            }
            offset += 8;
        }
        rest.iter()
            .position(|&b| b == b1 || b == b2)
            .map(|i| offset + i)
    }

    /// Find the index of the last occurrence of `b1` in `haystack`.
    #[inline]
    pub fn rfind_byte(haystack: &[u8], b1: u8) -> Option<usize> {
        let s1 = splat(b1);
        let aligned = haystack.len() & !7;
        if let Some(i) = haystack[aligned..].iter().rposition(|&b| b == b1) {
            return Some(aligned + i);
        }
        let mut pos = aligned;
        while pos >= 8 {
            pos -= 8;
            let chunk = &haystack[pos..pos + 8];
            // The borrow in `eq_mask` can only produce false positives above a real
            // match, so on any hit rescan the 8 bytes in reverse.
            if eq_mask(load(chunk), s1) & HI != 0
                && let Some(j) = chunk.iter().rposition(|&b| b == b1)
            {
                return Some(pos + j);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Find1 = fn(&[u8], u8) -> Option<usize>;
    type Find2 = fn(&[u8], u8, u8) -> Option<usize>;

    /// Both the active implementation and the portable fallback.
    const FIND: [Find1; 2] = [find_byte, portable::find_byte];
    const FIND2: [Find2; 2] = [find_byte2, portable::find_byte2];
    const RFIND: [Find1; 2] = [rfind_byte, portable::rfind_byte];

    #[test]
    fn test_find_byte_all_positions() {
        for f in FIND {
            for len in 0..100 {
                let mut buf = vec![b'x'; len];
                assert_eq!(f(&buf, b';'), None);
                for pos in 0..len {
                    buf[pos] = b';';
                    assert_eq!(f(&buf, b';'), Some(pos), "len={len} pos={pos}");
                    // A later match must not hide the first one.
                    if pos + 1 < len {
                        buf[len - 1] = b';';
                        assert_eq!(f(&buf, b';'), Some(pos));
                        buf[len - 1] = b'x';
                    }
                    buf[pos] = b'x';
                }
            }
        }
    }

    #[test]
    fn test_find_byte2_all_positions() {
        for f in FIND2 {
            for len in 0..100 {
                let mut buf = vec![b'x'; len];
                assert_eq!(f(&buf, b';', b'\n'), None);
                for pos in 0..len {
                    for b in *b";\n" {
                        buf[pos] = b;
                        assert_eq!(f(&buf, b';', b'\n'), Some(pos), "len={len} pos={pos}");
                        buf[pos] = b'x';
                    }
                }
            }
        }
    }

    #[test]
    fn test_find_byte2_borrow_edge_cases() {
        for f in FIND2 {
            // Test adjacent 0x01 and 0x00 after xor (e.g. target +/- 1 next to target)
            let buf = [b';' ^ 1, b'\n', b';', b'a', b'b', b'c', b'd', b'e'];
            assert_eq!(f(&buf, b';', b'\n'), Some(1));

            let buf2 = [b'a', b';' ^ 1, b';' ^ 1, b';', b'\n', b'x', b'y', b'z'];
            assert_eq!(f(&buf2, b';', b'\n'), Some(3));
        }
    }

    #[test]
    fn test_high_bytes() {
        // Bytes >= 0x80 (UTF-8) must not be confused with needles or with each other.
        let buf: Vec<u8> = "àèìòù€§".repeat(10).into_bytes();
        for f in FIND {
            assert_eq!(f(&buf, b';'), None);
            assert_eq!(f(&buf, 0xA7), buf.iter().position(|&b| b == 0xA7));
        }
        for f in RFIND {
            assert_eq!(f(&buf, 0xC3), buf.iter().rposition(|&b| b == 0xC3));
        }
    }

    #[test]
    fn test_rfind_byte() {
        for f in RFIND {
            for len in 0..100 {
                let mut buf = vec![b'a'; len];
                assert_eq!(f(&buf, b'\n'), None);
                for pos in 0..len {
                    buf[pos] = b'\n';
                    assert_eq!(f(&buf, b'\n'), Some(pos), "len={len} pos={pos}");
                    // Put an earlier newline too
                    if pos > 0 {
                        buf[0] = b'\n';
                        assert_eq!(f(&buf, b'\n'), Some(pos));
                        buf[0] = b'a';
                    }
                    buf[pos] = b'a';
                }
            }
        }
    }

    #[test]
    fn test_cursor_matches_naive() {
        let mut rng = 0x2545F4914F6CDD1Du64;
        for len in [0usize, 1, 5, 63, 64, 65, 127, 128, 200, 1000] {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    b";\n\"ab"[(rng % 5) as usize]
                })
                .collect();
            let mut cur = Cursor::new(&data, b';');
            let naive = |pos: usize, set: &[u8]| (pos..len).find(|&i| set.contains(&data[i]));
            // Monotonic positions, as used by the parser.
            for pos in 0..=len {
                assert_eq!(cur.next(pos, Want::DelimOrNewline), naive(pos, b";\n"));
                assert_eq!(cur.next(pos, Want::Quote), naive(pos, b"\""));
                assert_eq!(cur.next(pos, Want::QuoteOrNewline), naive(pos, b"\"\n"));
            }
            // Delimiter byte 0 must not match the zero padding of the last block.
            let mut cur0 = Cursor::new(&data, 0);
            assert_eq!(cur0.next(0, Want::DelimOrNewline), naive(0, b"\n"));
        }
    }
}
