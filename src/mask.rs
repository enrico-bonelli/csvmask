use crate::swar::find_byte;

/// Append `src` to `out`. Slices of up to 16 bytes (the common case for CSV fields and
/// separators) are copied with a few fixed-size overlapping moves instead of a `memcpy` call.
#[inline(always)]
pub fn extend_small(out: &mut Vec<u8>, src: &[u8]) {
    let n = src.len();
    if n > 16 {
        out.extend_from_slice(src);
        return;
    }
    out.reserve(16);
    // SAFETY: `out` has at least 16 bytes of spare capacity, and every read/write below
    // stays within `src[..n]` / the first `n` spare bytes of `out`.
    unsafe {
        let s = src.as_ptr();
        let d = out.as_mut_ptr().add(out.len());
        if n >= 8 {
            let a = s.cast::<u64>().read_unaligned();
            let b = s.add(n - 8).cast::<u64>().read_unaligned();
            d.cast::<u64>().write_unaligned(a);
            d.add(n - 8).cast::<u64>().write_unaligned(b);
        } else if n >= 4 {
            let a = s.cast::<u32>().read_unaligned();
            let b = s.add(n - 4).cast::<u32>().read_unaligned();
            d.cast::<u32>().write_unaligned(a);
            d.add(n - 4).cast::<u32>().write_unaligned(b);
        } else if n > 0 {
            *d = *s;
            *d.add(n / 2) = *s.add(n / 2);
            *d.add(n - 1) = *s.add(n - 1);
        }
        out.set_len(out.len() + n);
    }
}

#[derive(Clone, Debug)]
pub struct MaskConfig {
    pub mask_bytes: [u8; 4],
    pub mask_len: usize,
    pub keep_start: Option<usize>,
    pub keep_end: Option<usize>,
    pub delim_bytes: [u8; 4],
    pub delim_len: usize,
    pub mask_needs_quote_escape: bool,
    pub mask_needs_field_quoting: bool,
}

impl MaskConfig {
    pub fn new(
        mask_char: char,
        delim: char,
        keep_start: Option<usize>,
        keep_end: Option<usize>,
    ) -> Self {
        let mut mask_bytes = [0u8; 4];
        let mask_len = mask_char.encode_utf8(&mut mask_bytes).len();
        let mut delim_bytes = [0u8; 4];
        let delim_len = delim.encode_utf8(&mut delim_bytes).len();

        let mask_needs_quote_escape = mask_char == '"';
        let mask_needs_field_quoting =
            mask_char == '"' || mask_char == delim || mask_char == '\n' || mask_char == '\r';

        Self {
            mask_bytes,
            mask_len,
            keep_start,
            keep_end,
            delim_bytes,
            delim_len,
            mask_needs_quote_escape,
            mask_needs_field_quoting,
        }
    }

    #[inline(always)]
    pub fn plan(&self, len: usize) -> (usize, usize, usize) {
        if len == 0 {
            return (0, 0, 0);
        }
        let s = self.keep_start.unwrap_or(1);
        let e = self.keep_end.unwrap_or(1);
        if s + e < len {
            (s, len - s - e, e)
        } else {
            let avail = len - 1;
            let ks = s.min(avail);
            let ke = e.min(avail - ks);
            let mc = len - ks - ke;
            (ks, mc, ke)
        }
    }

    #[inline(always)]
    pub fn push_masks(&self, out: &mut Vec<u8>, count: usize) {
        if count == 0 {
            return;
        }
        if self.mask_len == 1 && count <= 16 {
            out.reserve(16);
            // SAFETY: 16 bytes of spare capacity were just reserved; only `count <= 16`
            // of them are marked initialized.
            unsafe {
                let d = out.as_mut_ptr().add(out.len());
                d.cast::<[u8; 16]>().write_unaligned([self.mask_bytes[0]; 16]);
                out.set_len(out.len() + count);
            }
        } else if self.mask_len == 1 {
            out.resize(out.len() + count, self.mask_bytes[0]);
        } else {
            let mb = &self.mask_bytes[..self.mask_len];
            out.reserve(count * self.mask_len);
            for _ in 0..count {
                out.extend_from_slice(mb);
            }
        }
    }
}

/// Mask a plain logical byte slice (no CSV quoting) into `out`.
#[inline(always)]
pub fn mask_plain_into(field: &[u8], out: &mut Vec<u8>, cfg: &MaskConfig) {
    if field.is_empty() {
        return;
    }
    if field.is_ascii() {
        let len = field.len();
        let (ks, mc, ke) = cfg.plan(len);
        extend_small(out, &field[..ks]);
        cfg.push_masks(out, mc);
        extend_small(out, &field[len - ke..]);
        return;
    }
    mask_non_ascii_into(field, out, cfg);
}

/// Slower path of `mask_plain_into` for fields containing non-ASCII bytes
/// (kept out of line so the ASCII path stays small enough to inline).
#[inline(never)]
fn mask_non_ascii_into(field: &[u8], out: &mut Vec<u8>, cfg: &MaskConfig) {
    if let Ok(s) = std::str::from_utf8(field) {
        let char_count = s.chars().count();
        let (ks, mc, ke) = cfg.plan(char_count);
        let start_byte = if ks == 0 {
            0
        } else {
            s.char_indices().nth(ks).map(|(i, _)| i).unwrap_or(s.len())
        };
        let end_byte = if ke == 0 {
            s.len()
        } else {
            s.char_indices()
                .rev()
                .nth(ke - 1)
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        extend_small(out, &field[..start_byte]);
        cfg.push_masks(out, mc);
        extend_small(out, &field[end_byte..]);
    } else {
        // Fallback for non-UTF-8 byte data: mask at byte granularity.
        let len = field.len();
        let (ks, mc, ke) = cfg.plan(len);
        extend_small(out, &field[..ks]);
        cfg.push_masks(out, mc);
        extend_small(out, &field[len - ke..]);
    }
}

/// Mask an unquoted CSV field into `out`.
#[inline]
pub fn mask_unquoted_into(
    field: &[u8],
    out: &mut Vec<u8>,
    cfg: &MaskConfig,
    scratch: &mut Vec<u8>,
) {
    if !cfg.mask_needs_field_quoting {
        mask_plain_into(field, out, cfg);
    } else {
        scratch.clear();
        mask_plain_into(field, scratch, cfg);
        write_quoted_escaped(scratch, out);
    }
}

/// Mask the inner bytes of a quoted CSV field (without surrounding quotes) and write `"..."` to `out`.
#[inline]
pub fn mask_quoted_into(
    inner: &[u8],
    out: &mut Vec<u8>,
    cfg: &MaskConfig,
    scratch_unescaped: &mut Vec<u8>,
    scratch_masked: &mut Vec<u8>,
) {
    if find_byte(inner, b'"').is_none() && !cfg.mask_needs_quote_escape {
        out.push(b'"');
        mask_plain_into(inner, out, cfg);
        out.push(b'"');
        return;
    }

    // Slow path: unescape `""` -> `"`, mask, then re-escape `"` -> `""`.
    scratch_unescaped.clear();
    let mut i = 0;
    while i < inner.len() {
        if let Some(rel) = find_byte(&inner[i..], b'"') {
            scratch_unescaped.extend_from_slice(&inner[i..i + rel]);
            scratch_unescaped.push(b'"');
            if i + rel + 1 < inner.len() && inner[i + rel + 1] == b'"' {
                i += rel + 2;
            } else {
                i += rel + 1;
            }
        } else {
            scratch_unescaped.extend_from_slice(&inner[i..]);
            break;
        }
    }

    scratch_masked.clear();
    mask_plain_into(scratch_unescaped, scratch_masked, cfg);
    write_quoted_escaped(scratch_masked, out);
}

#[inline]
fn write_quoted_escaped(raw: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    if find_byte(raw, b'"').is_none() {
        out.extend_from_slice(raw);
    } else {
        for &b in raw {
            if b == b'"' {
                out.extend_from_slice(b"\"\"");
            } else {
                out.push(b);
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask_str(s: &str, mask_char: char, ks: Option<usize>, ke: Option<usize>) -> String {
        let cfg = MaskConfig::new(mask_char, ';', ks, ke);
        let mut out = Vec::new();
        mask_plain_into(s.as_bytes(), &mut out, &cfg);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn test_default_masking() {
        assert_eq!(mask_str("", '*', None, None), "");
        assert_eq!(mask_str("a", '*', None, None), "*");
        assert_eq!(mask_str("ab", '*', None, None), "a*");
        assert_eq!(mask_str("abc", '*', None, None), "a*c");
        assert_eq!(mask_str("Mario", '*', None, None), "M***o");
        assert_eq!(
            mask_str("RSSMRA80A01H501U", '#', None, None),
            "R##############U"
        );
    }

    #[test]
    fn test_utf8_masking() {
        assert_eq!(mask_str("à", '*', None, None), "*");
        assert_eq!(mask_str("àè", '*', None, None), "à*");
        assert_eq!(mask_str("àèì", '*', None, None), "à*ì");
        assert_eq!(mask_str("Città", '*', None, None), "C***à");
        assert_eq!(mask_str("€1000", '•', None, None), "€•••0");
    }

    #[test]
    fn test_custom_keep_counts() {
        assert_eq!(mask_str("abcdef", '*', Some(0), Some(0)), "******");
        assert_eq!(mask_str("abcdef", '*', Some(2), Some(2)), "ab**ef");
        assert_eq!(mask_str("abc", '*', Some(2), Some(2)), "ab*");
        assert_eq!(mask_str("a", '*', Some(2), Some(2)), "*");
    }

    #[test]
    fn test_quoted_with_escaped_quotes() {
        let cfg = MaskConfig::new('*', ';', None, None);
        let mut out = Vec::new();
        let mut s1 = Vec::new();
        let mut s2 = Vec::new();
        // Logical string: "hello" (with outer quotes in CSV: """hello""")
        mask_quoted_into(b"\"\"hello\"\"", &mut out, &cfg, &mut s1, &mut s2);
        assert_eq!(String::from_utf8(out).unwrap(), "\"\"\"*****\"\"\"");
    }
}
