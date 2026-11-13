use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::mask::{MaskConfig, extend_small, mask_quoted_into, mask_unquoted_into};
use crate::swar::{Cursor, Want, find_byte, find_byte2, rfind_byte};

pub const CHUNK_SIZE: usize = 2 * 1024 * 1024; // 2 MiB

/// Maximum number of record-start checkpoints recorded per chunk (see `process_chunk`).
const MAX_CHECKPOINTS: usize = 64;

#[derive(Clone, Debug)]
pub enum ColumnSelector {
    Dense { mask: Vec<bool>, max_col: usize },
    Sparse { cols: Vec<usize>, max_col: usize },
}

impl ColumnSelector {
    /// Build from 1-based column indices.
    pub fn from_one_based(one_based: &[usize]) -> Self {
        let mut zero_based: Vec<usize> = one_based.iter().map(|&c| c - 1).collect();
        zero_based.sort_unstable();
        zero_based.dedup();
        let max_col = *zero_based.last().expect("at least one column required");
        if max_col <= 4096 {
            let mut mask = vec![false; max_col + 1];
            for c in zero_based {
                mask[c] = true;
            }
            Self::Dense { mask, max_col }
        } else {
            Self::Sparse {
                cols: zero_based,
                max_col,
            }
        }
    }

    #[inline(always)]
    pub fn should_mask(&self, col_idx: usize) -> bool {
        match self {
            Self::Dense { mask, .. } => col_idx < mask.len() && mask[col_idx],
            Self::Sparse { cols, max_col } => {
                col_idx <= *max_col && cols.binary_search(&col_idx).is_ok()
            }
        }
    }

    #[inline(always)]
    pub fn max_col(&self) -> usize {
        match self {
            Self::Dense { max_col, .. } | Self::Sparse { max_col, .. } => *max_col,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessOptions {
    pub keep_header: bool,
    pub threads: usize,
    pub input_size_hint: Option<u64>,
}

/// Reusable per-thread scratch state for `process_chunk`.
#[derive(Default)]
struct Scratch {
    s1: Vec<u8>,
    s2: Vec<u8>,
}

/// True if the quote at `idx` opens a quoted field (i.e. it sits at the start of a field).
#[inline(always)]
fn is_field_start(chunk: &[u8], idx: usize, row_start: usize, delim: &[u8]) -> bool {
    if idx == row_start || chunk[idx - 1] == b'\n' {
        return true;
    }
    let dlen = delim.len();
    if dlen == 1 {
        chunk[idx - 1] == delim[0]
    } else {
        idx >= dlen && &chunk[idx - dlen..idx] == delim
    }
}

/// Find the byte index right after the `\n` terminating the CSV row that starts at `start`.
///
/// `in_quotes` is the quote state at `start` (normally `false`; `true` is used to probe
/// a chunk that may begin in the middle of a multi-line quoted field).
/// Returns `None` if no terminating newline is found before the end of `chunk`.
#[inline]
fn find_row_end(chunk: &[u8], start: usize, delim: &[u8], mut in_quotes: bool) -> Option<usize> {
    let mut pos = start;
    while pos < chunk.len() {
        if !in_quotes {
            let idx = pos + find_byte2(&chunk[pos..], b'"', b'\n')?;
            if chunk[idx] == b'\n' {
                return Some(idx + 1);
            }
            if is_field_start(chunk, idx, start, delim) {
                in_quotes = true;
            }
            pos = idx + 1;
        } else {
            let idx = pos + find_byte(&chunk[pos..], b'"')?;
            if idx + 1 < chunk.len() && chunk[idx + 1] == b'"' {
                pos = idx + 2;
            } else {
                in_quotes = false;
                pos = idx + 1;
            }
        }
    }
    None
}

/// Find the closing quote of a quoted field whose opening quote is at `open`.
#[inline(always)]
fn find_closing_quote(chunk: &[u8], open: usize) -> Option<usize> {
    let mut scan = open + 1;
    loop {
        let q = scan + find_byte(&chunk[scan..], b'"')?;
        if q + 1 < chunk.len() && chunk[q + 1] == b'"' {
            scan = q + 2;
        } else {
            return Some(q);
        }
    }
}

/// `find_row_end` (starting outside quotes) driven by a structural `Cursor`.
#[inline(always)]
fn row_end_cur(cur: &mut Cursor, chunk: &[u8], start: usize, delim: u8) -> Option<usize> {
    let mut pos = start;
    let mut in_quotes = false;
    loop {
        if !in_quotes {
            let idx = cur.next(pos, Want::QuoteOrNewline)?;
            if chunk[idx] == b'\n' {
                return Some(idx + 1);
            }
            if idx == start || chunk[idx - 1] == delim || chunk[idx - 1] == b'\n' {
                in_quotes = true;
            }
            pos = idx + 1;
        } else {
            let idx = cur.next(pos, Want::Quote)?;
            if idx + 1 < chunk.len() && chunk[idx + 1] == b'"' {
                pos = idx + 2;
            } else {
                in_quotes = false;
                pos = idx + 1;
            }
        }
    }
}

/// `find_closing_quote` driven by a structural `Cursor`.
#[inline(always)]
fn closing_quote_cur(cur: &mut Cursor, chunk: &[u8], open: usize) -> Option<usize> {
    let mut scan = open + 1;
    loop {
        let q = cur.next(scan, Want::Quote)?;
        if q + 1 < chunk.len() && chunk[q + 1] == b'"' {
            scan = q + 2;
        } else {
            return Some(q);
        }
    }
}

/// Process a CSV chunk that starts at a record boundary, using a single-byte delimiter.
///
/// Bytes that are output verbatim are not copied field by field: `chunk[copy_from..pos]`
/// is a pending verbatim span, flushed with a single copy only when a masked field has
/// to be written (or at the end). This avoids one small `memcpy` per field.
///
/// See `process_chunk` for the meaning of `at_eof`, the return value and `checkpoints`.
#[allow(clippy::too_many_arguments)]
fn process_chunk_byte_delim(
    chunk: &[u8],
    out: &mut Vec<u8>,
    delim: u8,
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    skip_first_row: bool,
    at_eof: bool,
    scratch: &mut Scratch,
    checkpoints: &mut Vec<(usize, usize)>,
) -> Option<usize> {
    out.clear();
    checkpoints.clear();
    if chunk.is_empty() {
        return None;
    }
    out.reserve(chunk.len() + 64);
    let n = chunk.len();
    let delim_slice = [delim];

    let mut pos = 0;
    if skip_first_row {
        match find_row_end(chunk, 0, &delim_slice, false) {
            Some(e) => pos = e,
            None if !at_eof => return Some(0),
            None => {
                out.extend_from_slice(chunk);
                return None;
            }
        }
    }

    let max_col = cols.max_col();
    let mut cur = Cursor::new(chunk, delim);
    let mut col_idx = 0usize;
    let mut copy_from = 0usize;
    let mut row_start = pos;
    // Length `out` will have once flushed up to `row_start`.
    let mut row_out = pos;
    checkpoints.push((pos, row_out));

    // Write the pending verbatim span up to `$end` (the caller then moves `copy_from`).
    macro_rules! flush_to {
        ($end:expr) => {
            extend_small(out, &chunk[copy_from..$end])
        };
    }
    // Called right after `pos` has been advanced past a row terminator.
    macro_rules! end_row {
        () => {{
            col_idx = 0;
            row_start = pos;
            row_out = out.len() + (pos - copy_from);
            if checkpoints.len() < MAX_CHECKPOINTS {
                checkpoints.push((pos, row_out));
            }
        }};
    }
    // The current row continues past the end of the chunk: emit everything before it
    // and report where it starts, unless this is the final chunk of the input.
    macro_rules! incomplete_unless_eof {
        () => {
            if !at_eof {
                if copy_from <= row_start {
                    out.extend_from_slice(&chunk[copy_from..row_start]);
                } else {
                    out.truncate(row_out);
                }
                return Some(row_start);
            }
        };
    }

    while pos < n {
        if col_idx > max_col {
            match row_end_cur(&mut cur, chunk, pos, delim) {
                Some(row_end) => {
                    pos = row_end;
                    end_row!();
                }
                None => {
                    incomplete_unless_eof!();
                    pos = n;
                }
            }
            continue;
        }

        if chunk[pos] != b'"' {
            let (end, hit_delim) = match cur.next(pos, Want::DelimOrNewline) {
                Some(idx) => (idx, chunk[idx] == delim),
                None => {
                    incomplete_unless_eof!();
                    (n, false)
                }
            };

            if cols.should_mask(col_idx) {
                // Keep a `\r` of a CRLF terminator out of the masked value.
                let field_end = if !hit_delim && end < n && end > pos && chunk[end - 1] == b'\r' {
                    end - 1
                } else {
                    end
                };
                flush_to!(pos);
                mask_unquoted_into(&chunk[pos..field_end], out, cfg, &mut scratch.s1);
                copy_from = field_end;
            }
            if hit_delim {
                pos = end + 1;
                col_idx += 1;
            } else {
                pos = if end < n { end + 1 } else { end };
                end_row!();
            }
        } else {
            // Quoted field
            let closing_q = closing_quote_cur(&mut cur, chunk, pos);
            if closing_q.is_none_or(|q| q + 1 == n) {
                // Unclosed quote, or a quote at the very end that may be half of an `""` escape.
                incomplete_unless_eof!();
            }

            if let Some(q) = closing_q {
                let (end, hit_delim) = match cur.next(q + 1, Want::DelimOrNewline) {
                    Some(idx) => (idx, chunk[idx] == delim),
                    None => {
                        incomplete_unless_eof!();
                        (n, false)
                    }
                };
                if cols.should_mask(col_idx) {
                    flush_to!(pos);
                    mask_quoted_into(&chunk[pos + 1..q], out, cfg, &mut scratch.s1, &mut scratch.s2);
                    copy_from = q + 1;
                }
                pos = if end < n { end + 1 } else { end };
                if hit_delim {
                    col_idx += 1;
                } else {
                    end_row!();
                }
            } else {
                // Unclosed quote at EOF
                if cols.should_mask(col_idx) {
                    flush_to!(pos);
                    mask_quoted_into(&chunk[pos + 1..], out, cfg, &mut scratch.s1, &mut scratch.s2);
                    copy_from = n;
                }
                pos = n;
                end_row!();
            }
        }
    }
    out.extend_from_slice(&chunk[copy_from..]);
    None
}

/// Find the end of an unquoted field for a multi-byte delimiter: `(index, hit_delim)`.
#[inline(always)]
fn find_field_end_utf8(chunk: &[u8], from: usize, delim_bytes: &[u8]) -> Option<(usize, bool)> {
    let d0 = delim_bytes[0];
    let dlen = delim_bytes.len();
    let mut scan = from;
    loop {
        let idx = scan + find_byte2(&chunk[scan..], d0, b'\n')?;
        if chunk[idx] == b'\n' {
            return Some((idx, false));
        }
        if idx + dlen <= chunk.len() && &chunk[idx..idx + dlen] == delim_bytes {
            return Some((idx, true));
        }
        scan = idx + 1;
    }
}

/// General CSV chunk processor for multi-byte UTF-8 delimiters.
///
/// See `process_chunk` for the meaning of `at_eof`, the return value and `checkpoints`.
#[allow(clippy::too_many_arguments)]
fn process_chunk_utf8_delim(
    chunk: &[u8],
    out: &mut Vec<u8>,
    delim_bytes: &[u8],
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    skip_first_row: bool,
    at_eof: bool,
    scratch: &mut Scratch,
    checkpoints: &mut Vec<(usize, usize)>,
) -> Option<usize> {
    out.clear();
    checkpoints.clear();
    if chunk.is_empty() {
        return None;
    }
    out.reserve(chunk.len() + 64);

    let n = chunk.len();
    let dlen = delim_bytes.len();
    let mut pos = 0usize;
    let mut col_idx = 0usize;
    let mut skipping_header = skip_first_row;
    let mut row_start = 0usize;
    let mut row_out = 0usize;
    checkpoints.push((0, 0));

    macro_rules! end_row {
        () => {{
            col_idx = 0;
            skipping_header = false;
            row_start = pos;
            row_out = out.len();
            if checkpoints.len() < MAX_CHECKPOINTS {
                checkpoints.push((pos, row_out));
            }
        }};
    }
    macro_rules! incomplete_unless_eof {
        () => {
            if !at_eof {
                out.truncate(row_out);
                return Some(row_start);
            }
        };
    }

    while pos < n {
        if chunk[pos] != b'"' {
            let (end, hit_delim) = match find_field_end_utf8(chunk, pos, delim_bytes) {
                Some(r) => r,
                None => {
                    incomplete_unless_eof!();
                    (n, false)
                }
            };

            let sep_len = if hit_delim {
                dlen
            } else if end < n {
                1
            } else {
                0
            };

            if skipping_header || !cols.should_mask(col_idx) {
                out.extend_from_slice(&chunk[pos..end + sep_len]);
            } else if hit_delim {
                mask_unquoted_into(&chunk[pos..end], out, cfg, &mut scratch.s1);
                out.extend_from_slice(delim_bytes);
            } else if end < n {
                if end > pos && chunk[end - 1] == b'\r' {
                    mask_unquoted_into(&chunk[pos..end - 1], out, cfg, &mut scratch.s1);
                    out.extend_from_slice(b"\r\n");
                } else {
                    mask_unquoted_into(&chunk[pos..end], out, cfg, &mut scratch.s1);
                    out.push(b'\n');
                }
            } else {
                mask_unquoted_into(&chunk[pos..end], out, cfg, &mut scratch.s1);
            }

            pos = end + sep_len;
            if hit_delim {
                col_idx += 1;
            } else {
                end_row!();
            }
        } else {
            let closing_q = find_closing_quote(chunk, pos);
            if closing_q.is_none_or(|q| q + 1 == n) {
                incomplete_unless_eof!();
            }

            if let Some(q) = closing_q {
                let (end, hit_delim) = match find_field_end_utf8(chunk, q + 1, delim_bytes) {
                    Some(r) => r,
                    None => {
                        incomplete_unless_eof!();
                        (n, false)
                    }
                };
                let sep_len = if hit_delim {
                    dlen
                } else if end < n {
                    1
                } else {
                    0
                };
                let copy_end = end + sep_len;

                if skipping_header || !cols.should_mask(col_idx) {
                    out.extend_from_slice(&chunk[pos..copy_end]);
                } else {
                    mask_quoted_into(&chunk[pos + 1..q], out, cfg, &mut scratch.s1, &mut scratch.s2);
                    out.extend_from_slice(&chunk[q + 1..copy_end]);
                }

                pos = copy_end;
                if hit_delim {
                    col_idx += 1;
                } else {
                    end_row!();
                }
            } else {
                if skipping_header || !cols.should_mask(col_idx) {
                    out.extend_from_slice(&chunk[pos..]);
                } else {
                    mask_quoted_into(&chunk[pos + 1..], out, cfg, &mut scratch.s1, &mut scratch.s2);
                }
                pos = n;
                end_row!();
            }
        }
    }
    None
}

/// Process a CSV chunk that starts at a record boundary into `out`.
///
/// If `at_eof` is false and the last record of the chunk is incomplete (an unterminated
/// quoted field, or a missing final newline), that record is left out of `out` and its
/// start offset in `chunk` is returned, so the caller can retry it with more data.
///
/// `checkpoints` receives `(chunk_offset, out_len)` pairs for the first record starts
/// of the chunk; they allow reusing a suffix of `out` when it later turns out that the
/// chunk actually began inside a quoted field of the previous chunk.
#[allow(clippy::too_many_arguments)]
fn process_chunk(
    chunk: &[u8],
    out: &mut Vec<u8>,
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    skip_first_row: bool,
    at_eof: bool,
    scratch: &mut Scratch,
    checkpoints: &mut Vec<(usize, usize)>,
) -> Option<usize> {
    if cfg.delim_len == 1 {
        process_chunk_byte_delim(
            chunk,
            out,
            cfg.delim_bytes[0],
            cols,
            cfg,
            skip_first_row,
            at_eof,
            scratch,
            checkpoints,
        )
    } else {
        process_chunk_utf8_delim(
            chunk,
            out,
            &cfg.delim_bytes[..cfg.delim_len],
            cols,
            cfg,
            skip_first_row,
            at_eof,
            scratch,
            checkpoints,
        )
    }
}

/// Read up to `target_len` bytes into `buf` (appending after existing carryover bytes),
/// stopping on EOF or when `buf.len() >= target_len`.
fn fill_buffer<R: Read>(reader: &mut R, buf: &mut Vec<u8>, target_len: usize) -> io::Result<bool> {
    if buf.len() >= target_len {
        return Ok(false);
    }
    let old_len = buf.len();
    buf.resize(target_len, 0);
    let mut filled = old_len;
    let mut hit_eof = false;
    while filled < target_len {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                hit_eof = true;
                break;
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                buf.truncate(filled);
                return Err(e);
            }
        }
    }
    buf.truncate(filled);
    Ok(hit_eof)
}

/// Read the next raw chunk into `in_buf`, cut right after its last `\n` (quotes are *not*
/// considered here: that is resolved later, see `Stitcher`). Bytes after the cut are moved
/// into `carry` and prepended to the next chunk.
///
/// Returns `None` when there is no more input, otherwise `Some(is_last)`.
fn read_next_chunk<R: Read>(
    reader: &mut R,
    in_buf: &mut Vec<u8>,
    carry: &mut Vec<u8>,
) -> io::Result<Option<bool>> {
    in_buf.clear();
    in_buf.extend_from_slice(carry);
    carry.clear();

    let mut search_from = 0;
    loop {
        let target = in_buf.len() + CHUNK_SIZE;
        let hit_eof = fill_buffer(reader, in_buf, target)?;
        if in_buf.is_empty() {
            return Ok(None);
        }
        if hit_eof {
            return Ok(Some(true));
        }
        if let Some(rel) = rfind_byte(&in_buf[search_from..], b'\n') {
            let split_at = search_from + rel + 1;
            carry.extend_from_slice(&in_buf[split_at..]);
            in_buf.truncate(split_at);
            return Ok(Some(false));
        }
        // No newline at all in this chunk (single huge line): keep reading.
        search_from = in_buf.len();
    }
}

pub fn run<R: Read + Send, W: Write>(
    mut reader: R,
    mut writer: W,
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    opts: &ProcessOptions,
) -> io::Result<()> {
    let use_single_thread = opts.threads <= 1
        || opts
            .input_size_hint
            .is_some_and(|sz| sz < (CHUNK_SIZE as u64));

    if use_single_thread {
        return run_single_threaded(&mut reader, &mut writer, cols, cfg, opts);
    }

    // Read the first chunk: if the entire input fits in it (e.g. small stdin pipe),
    // avoid the thread pool overhead.
    let mut first_in = Vec::with_capacity(CHUNK_SIZE);
    let mut carry = Vec::with_capacity(64 * 1024);
    let Some(is_last) = read_next_chunk(&mut reader, &mut first_in, &mut carry)? else {
        return Ok(());
    };
    if is_last {
        let mut out = Vec::with_capacity(first_in.len() + 64);
        process_chunk(
            &first_in,
            &mut out,
            cols,
            cfg,
            opts.keep_header,
            true,
            &mut Scratch::default(),
            &mut Vec::new(),
        );
        writer.write_all(&out)?;
        return writer.flush();
    }

    run_parallel(reader, writer, cols, cfg, opts, first_in, carry)
}

fn run_single_threaded<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    opts: &ProcessOptions,
) -> io::Result<()> {
    let mut in_buf = Vec::with_capacity(CHUNK_SIZE);
    let mut out_buf = Vec::with_capacity(CHUNK_SIZE);
    let mut carry = Vec::with_capacity(64 * 1024);
    let mut spare = Vec::new();
    let mut scratch = Scratch::default();
    let mut checkpoints = Vec::with_capacity(MAX_CHECKPOINTS);
    let mut header_pending = opts.keep_header;

    // Every chunk starts at a record boundary here, because an incomplete trailing
    // record is pushed back in front of the next chunk.
    while let Some(is_last) = read_next_chunk(reader, &mut in_buf, &mut carry)? {
        let tail = process_chunk(
            &in_buf,
            &mut out_buf,
            cols,
            cfg,
            header_pending,
            is_last,
            &mut scratch,
            &mut checkpoints,
        );
        writer.write_all(&out_buf)?;
        header_pending = header_pending && tail == Some(0);
        if let Some(t) = tail {
            spare.clear();
            spare.extend_from_slice(&in_buf[t..]);
            spare.extend_from_slice(&carry);
            std::mem::swap(&mut carry, &mut spare);
        }
    }
    writer.flush()
}

struct Buffers {
    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    checkpoints: Vec<(usize, usize)>,
}

impl Buffers {
    fn new(in_buf: Vec<u8>) -> Self {
        Self {
            in_buf,
            out_buf: Vec::with_capacity(CHUNK_SIZE),
            checkpoints: Vec::with_capacity(MAX_CHECKPOINTS),
        }
    }
}

struct Job {
    seq: usize,
    is_last: bool,
    bufs: Buffers,
}

struct Completed {
    seq: usize,
    is_last: bool,
    /// Start of the incomplete trailing record left out of `out_buf`, if any.
    tail: Option<usize>,
    bufs: Buffers,
}

/// Sequential (in-order) stage that reconciles the speculative per-chunk results.
///
/// Workers process every chunk assuming it begins at a record boundary. That is wrong only
/// when the previous chunk ended inside a multi-line quoted field; in that case the
/// pending record bytes are held in `carry` and stitched to the next chunk here.
struct Stitcher<'a> {
    cols: &'a ColumnSelector,
    cfg: &'a MaskConfig,
    keep_header: bool,
    /// Raw bytes of an incomplete record (starting at a record boundary).
    carry: Vec<u8>,
    /// Whether `carry` holds the (incomplete) header row that must be kept unmasked.
    carry_is_header: bool,
    combined: Vec<u8>,
    out: Vec<u8>,
    scratch: Scratch,
    checkpoints: Vec<(usize, usize)>,
}

impl<'a> Stitcher<'a> {
    fn new(cols: &'a ColumnSelector, cfg: &'a MaskConfig, keep_header: bool) -> Self {
        Self {
            cols,
            cfg,
            keep_header,
            carry: Vec::new(),
            carry_is_header: false,
            combined: Vec::new(),
            out: Vec::new(),
            scratch: Scratch::default(),
            checkpoints: Vec::new(),
        }
    }

    /// Process `self.combined` serially and write the result.
    fn process_combined<W: Write>(
        &mut self,
        at_eof: bool,
        writer: &mut W,
    ) -> io::Result<Option<usize>> {
        let tail = process_chunk(
            &self.combined,
            &mut self.out,
            self.cols,
            self.cfg,
            self.carry_is_header,
            at_eof,
            &mut self.scratch,
            &mut self.checkpoints,
        );
        writer.write_all(&self.out)?;
        Ok(tail)
    }

    fn handle<W: Write>(&mut self, done: &Completed, writer: &mut W) -> io::Result<()> {
        let chunk = &done.bufs.in_buf;

        if self.carry.is_empty() {
            // The chunk really starts at a record boundary: the worker output is valid.
            writer.write_all(&done.bufs.out_buf)?;
            if let Some(t) = done.tail {
                self.carry.extend_from_slice(&chunk[t..]);
                self.carry_is_header = done.seq == 0 && self.keep_header && t == 0;
            }
            return Ok(());
        }

        // The previous chunk ended inside a quoted field: find where that record ends
        // in this chunk, assuming it starts inside quotes.
        let delim = &self.cfg.delim_bytes[..self.cfg.delim_len];
        match find_row_end(chunk, 0, delim, true) {
            None if !done.is_last => {
                // The whole chunk still belongs to the pending record.
                self.carry.extend_from_slice(chunk);
                return Ok(());
            }
            None => {}
            Some(rec_end) => {
                // Fast path: if the worker's speculative parse passed through a record
                // boundary at `rec_end`, everything after it is already correct.
                if let Some(&(_, out_off)) =
                    done.bufs.checkpoints.iter().find(|&&(p, _)| p == rec_end)
                {
                    self.combined.clear();
                    self.combined.extend_from_slice(&self.carry);
                    self.combined.extend_from_slice(&chunk[..rec_end]);
                    let tail = process_chunk(
                        &self.combined,
                        &mut self.out,
                        self.cols,
                        self.cfg,
                        self.carry_is_header,
                        false,
                        &mut self.scratch,
                        &mut self.checkpoints,
                    );
                    if tail.is_none() {
                        writer.write_all(&self.out)?;
                        writer.write_all(&done.bufs.out_buf[out_off..])?;
                        self.carry.clear();
                        self.carry_is_header = false;
                        if let Some(t) = done.tail {
                            self.carry.extend_from_slice(&chunk[t..]);
                        }
                        return Ok(());
                    }
                }
            }
        }

        // Slow path (rare): re-process pending record + whole chunk serially.
        self.combined.clear();
        self.combined.extend_from_slice(&self.carry);
        self.combined.extend_from_slice(chunk);
        let tail = self.process_combined(done.is_last, writer)?;
        self.carry.clear();
        if let Some(t) = tail {
            self.carry.extend_from_slice(&self.combined[t..]);
        }
        self.carry_is_header = self.carry_is_header && tail == Some(0);
        Ok(())
    }

    /// Flush a record still pending at end of input.
    fn finish<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if self.carry.is_empty() {
            return Ok(());
        }
        self.combined.clear();
        self.combined.extend_from_slice(&self.carry);
        self.process_combined(true, writer)?;
        self.carry.clear();
        Ok(())
    }
}

fn run_parallel<R: Read + Send, W: Write>(
    mut reader: R,
    mut writer: W,
    cols: &ColumnSelector,
    cfg: &MaskConfig,
    opts: &ProcessOptions,
    first_in: Vec<u8>,
    mut carry: Vec<u8>,
) -> io::Result<()> {
    let workers = opts.threads.max(2);
    let pool_size = workers * 2 + 2;

    let (pool_tx, pool_rx) = mpsc::channel::<Buffers>();
    let (work_tx, work_rx) = mpsc::channel::<Job>();
    let (done_tx, done_rx) = mpsc::channel::<Completed>();

    // Seed the pool (minus the buffer set already used by the first chunk).
    for _ in 1..pool_size {
        let _ = pool_tx.send(Buffers::new(Vec::with_capacity(CHUNK_SIZE)));
    }

    let work_rx = Arc::new(Mutex::new(work_rx));

    thread::scope(|scope| {
        // Spawn worker threads.
        for _ in 0..workers {
            let work_rx = Arc::clone(&work_rx);
            let done_tx = done_tx.clone();
            scope.spawn(move || {
                let mut scratch = Scratch::default();
                loop {
                    let job = {
                        let guard = work_rx.lock().unwrap();
                        guard.recv()
                    };
                    let Ok(mut job) = job else {
                        break;
                    };
                    let skip_header = job.seq == 0 && opts.keep_header;
                    let bufs = &mut job.bufs;
                    let tail = process_chunk(
                        &bufs.in_buf,
                        &mut bufs.out_buf,
                        cols,
                        cfg,
                        skip_header,
                        job.is_last,
                        &mut scratch,
                        &mut bufs.checkpoints,
                    );
                    let done = Completed {
                        seq: job.seq,
                        is_last: job.is_last,
                        tail,
                        bufs: job.bufs,
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            });
        }
        drop(done_tx);

        // Spawn reader thread.
        let reader_handle = scope.spawn(move || -> io::Result<()> {
            let first = Job {
                seq: 0,
                is_last: false,
                bufs: Buffers::new(first_in),
            };
            if work_tx.send(first).is_err() {
                return Ok(());
            }
            let mut seq = 1usize;
            while let Ok(mut bufs) = pool_rx.recv() {
                let Some(is_last) = read_next_chunk(&mut reader, &mut bufs.in_buf, &mut carry)?
                else {
                    break;
                };
                if work_tx.send(Job { seq, is_last, bufs }).is_err() {
                    break;
                }
                seq += 1;
                if is_last {
                    break;
                }
            }
            Ok(())
        });

        // Writer runs on the current thread, preserving exact chunk order.
        let mut stitcher = Stitcher::new(cols, cfg, opts.keep_header);
        let mut next_seq = 0usize;
        let mut pending: BTreeMap<usize, Completed> = BTreeMap::new();
        let mut write_err: Option<io::Error> = None;

        while let Ok(done) = done_rx.recv() {
            if write_err.is_some() {
                let _ = pool_tx.send(done.bufs);
                continue;
            }
            pending.insert(done.seq, done);
            while let Some(done) = pending.remove(&next_seq) {
                if write_err.is_none()
                    && let Err(e) = stitcher.handle(&done, &mut writer)
                {
                    write_err = Some(e);
                }
                let _ = pool_tx.send(done.bufs);
                next_seq += 1;
            }
        }

        drop(pool_tx);
        let reader_res = reader_handle
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("reader thread panicked")));

        if let Some(e) = write_err {
            return Err(e);
        }
        reader_res?;
        stitcher.finish(&mut writer)?;
        writer.flush()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run_mem(
        input: &str,
        cols: &[usize],
        delim: char,
        mask_char: char,
        keep_header: bool,
        threads: usize,
    ) -> String {
        let sel = ColumnSelector::from_one_based(cols);
        let cfg = MaskConfig::new(mask_char, delim, None, None);
        let opts = ProcessOptions {
            keep_header,
            threads,
            input_size_hint: Some(input.len() as u64),
        };
        let mut out = Vec::new();
        run(Cursor::new(input.as_bytes()), &mut out, &sel, &cfg, &opts).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn test_basic_csv_semicolon() {
        let csv = "id;nome;cognome;email\n1;Mario;Rossi;mario@example.com\n2;Anna;Bianchi;a@b.it\n";
        let masked = run_mem(csv, &[2, 4], ';', '*', true, 1);
        assert_eq!(
            masked,
            "id;nome;cognome;email\n1;M***o;Rossi;m***************m\n2;A**a;Bianchi;a****t\n"
        );
    }

    #[test]
    fn test_mask_all_rows_when_no_keep_header() {
        let csv = "Mario;Rossi\nLuigi;Verdi\n";
        let masked = run_mem(csv, &[1], ';', '*', false, 1);
        assert_eq!(masked, "M***o;Rossi\nL***i;Verdi\n");
    }

    #[test]
    fn test_crlf_and_quoted_multiline() {
        let csv = "1;\"Mario\nRossi\";\"a\"\"b\"\r\n2;\"X\";ok\r\n";
        let masked = run_mem(csv, &[2, 3], ';', '#', false, 1);
        assert_eq!(
            masked,
            "1;\"M#########i\";\"a#b\"\r\n2;\"#\";o#\r\n"
        );
    }

    #[test]
    fn test_utf8_delimiter() {
        let csv = "Mario§Rossi§Roma\n";
        let masked = run_mem(csv, &[1, 3], '§', '*', false, 1);
        assert_eq!(masked, "M***o§Rossi§R**a\n");
    }

    #[test]
    fn test_missing_columns_and_empty_fields() {
        let csv = "a;b\n;x;y\nabc\n";
        let masked = run_mem(csv, &[1, 5], ';', '*', false, 1);
        assert_eq!(masked, "*;b\n;x;y\na*c\n");
    }

    #[test]
    fn test_parallel_matches_single_threaded_on_large_quoted_input() {
        // Build > 5 MiB input with multiline quotes and CRLF to exercise multi-chunk parallel splitting
        let mut big = String::with_capacity(6 * 1024 * 1024);
        big.push_str("id;name;notes;city\n");
        for i in 0..100_000 {
            if i % 17 == 0 {
                big.push_str(&format!("{};Mario{};\"line1\nline2\";\"Città\"\"{}\"\r\n", i, i, i));
            } else {
                big.push_str(&format!("{};Anna{};note{};Milano\n", i, i, i));
            }
        }
        let sel = ColumnSelector::from_one_based(&[2, 4]);
        let cfg = MaskConfig::new('*', ';', None, None);

        let mut out_single = Vec::new();
        run(
            Cursor::new(big.as_bytes()),
            &mut out_single,
            &sel,
            &cfg,
            &ProcessOptions {
                keep_header: true,
                threads: 1,
                input_size_hint: Some(big.len() as u64),
            },
        )
        .unwrap();

        let mut out_par = Vec::new();
        run(
            Cursor::new(big.as_bytes()),
            &mut out_par,
            &sel,
            &cfg,
            &ProcessOptions {
                keep_header: true,
                threads: 4,
                input_size_hint: None, // force parallel path even without size hint
            },
        )
        .unwrap();

        assert_eq!(out_single, out_par);
    }

    /// Reference: the whole input processed as a single chunk.
    fn reference(input: &[u8], sel: &ColumnSelector, cfg: &MaskConfig, keep_header: bool) -> Vec<u8> {
        let mut out = Vec::new();
        process_chunk(
            input,
            &mut out,
            sel,
            cfg,
            keep_header,
            true,
            &mut Scratch::default(),
            &mut Vec::new(),
        );
        out
    }

    fn check_all_paths(input: &[u8], cols: &[usize], delim: char, keep_header: bool) {
        let sel = ColumnSelector::from_one_based(cols);
        let cfg = MaskConfig::new('*', delim, None, None);
        let expected = reference(input, &sel, &cfg, keep_header);
        for threads in [1, 3, 8] {
            let mut out = Vec::new();
            run(
                Cursor::new(input),
                &mut out,
                &sel,
                &cfg,
                &ProcessOptions {
                    keep_header,
                    threads,
                    input_size_hint: None,
                },
            )
            .unwrap();
            assert!(
                out == expected,
                "mismatch with threads={threads}, delim={delim:?}, keep_header={keep_header}"
            );
        }
    }

    /// Simple deterministic PRNG (xorshift) to avoid external dependencies.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }
    }

    fn random_csv(seed: u64, rows: usize, delim: &str) -> Vec<u8> {
        let mut rng = Rng(seed);
        let words = ["Mario", "Città", "a", "", "x\"y", "line\nbreak", "€", "\r", "q\"\""];
        let mut s = String::new();
        for r in 0..rows {
            let ncols = 1 + rng.below(6);
            for c in 0..ncols {
                if c > 0 {
                    s.push_str(delim);
                }
                let w = words[rng.below(words.len() as u64)];
                match rng.below(4) {
                    // Long multi-line quoted field: makes chunk cuts land inside quotes often.
                    0 => {
                        s.push('"');
                        for _ in 0..rng.below(400) {
                            s.push_str(&w.replace('"', "\"\""));
                            s.push(if rng.below(5) == 0 { '\n' } else { ' ' });
                        }
                        s.push('"');
                    }
                    1 => {
                        s.push('"');
                        s.push_str(&w.replace('"', "\"\""));
                        s.push('"');
                    }
                    _ => s.push_str(&w.replace(['\n', '"', '\r'], "_")),
                }
            }
            s.push_str(if r % 3 == 0 { "\r\n" } else { "\n" });
        }
        s.into_bytes()
    }

    #[test]
    fn test_parallel_random_multiline_quoted() {
        for seed in 1..4u64 {
            let csv = random_csv(seed * 7919, 40_000, ";");
            assert!(csv.len() > 3 * CHUNK_SIZE);
            check_all_paths(&csv, &[1, 3, 4], ';', seed % 2 == 0);
        }
    }

    #[test]
    fn test_parallel_random_utf8_delim() {
        let csv = random_csv(424242, 30_000, "§");
        assert!(csv.len() > 3 * CHUNK_SIZE);
        check_all_paths(&csv, &[2, 5], '§', true);
    }

    #[test]
    fn test_parallel_giant_quoted_field_and_header() {
        // Header row and one data field each spanning multiple chunks.
        let mut s = String::from("\"h");
        for _ in 0..300_000 {
            s.push_str("hdr \"\" \n");
        }
        s.push_str("\";col2\n");
        for i in 0..50_000 {
            s.push_str(&format!("{i};Mario;\"Rossi\"\n"));
        }
        s.push_str("1;\"");
        for _ in 0..400_000 {
            s.push_str("big\nfield ");
        }
        s.push_str("\";tail\n2;Anna;\"Verdi\""); // no trailing newline
        check_all_paths(s.as_bytes(), &[1, 2], ';', true);
        check_all_paths(s.as_bytes(), &[2], ';', false);
    }

    #[test]
    fn test_parallel_unclosed_quote_at_eof() {
        let mut s = String::new();
        for i in 0..200_000 {
            s.push_str(&format!("{i};Mario;Rossi\n"));
        }
        s.push_str("x;\"unclosed\nstill ");
        check_all_paths(s.as_bytes(), &[2], ';', false);
    }
}
