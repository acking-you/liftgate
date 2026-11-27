//! Bounded, atomic, content-addressed local file store for truncated prompt, tool, and web payloads.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use liftgate_types::session_key::{TruncationSpillSink, format_content_addressed_spill_filename};
use thiserror::Error;
use uuid::Uuid;

use crate::platform::canonicalize_clean;

/// Default cap on retained spill files inside `~/.liftgate/spills`.
pub const DEFAULT_MAX_SPILL_FILES: usize = 256;

/// Default cumulative disk cap (`256 MiB`) across retained spill files.
pub const DEFAULT_MAX_SPILL_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

const _: () = {
    assert!(
        DEFAULT_MAX_SPILL_FILES >= 16,
        "default spill file retention cap must accommodate multi-tool turns"
    );
    assert!(
        DEFAULT_MAX_SPILL_TOTAL_BYTES >= 16 * 1024 * 1024,
        "default spill byte cap must accommodate large command logs and transcripts"
    );
};

/// Typed filesystem error returned by [`SpillStore::try_spill`].
#[derive(Debug, Error)]
pub enum SpillStoreError {
    #[error("failed to create spill directory `{}`", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to create temporary spill file `{}`", path.display())]
    CreateTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to write temporary spill file `{}`", path.display())]
    WriteTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to persist spill file `{}`", path.display())]
    Persist {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct TempFileCleanupGuard {
    path: Option<PathBuf>,
}

impl TempFileCleanupGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileCleanupGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Bounded local directory store that persists full lossless payloads before truncation.
#[derive(Clone, Debug)]
pub struct SpillStore {
    dir: PathBuf,
    max_files: usize,
    max_total_bytes: u64,
}

impl SpillStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_files: DEFAULT_MAX_SPILL_FILES,
            max_total_bytes: DEFAULT_MAX_SPILL_TOTAL_BYTES,
        }
    }

    #[must_use]
    pub fn with_limits(dir: impl Into<PathBuf>, max_files: usize, max_total_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            max_files: max_files.max(1),
            max_total_bytes: max_total_bytes.max(1024),
        }
    }

    /// Atomically writes `full_content` into a content-addressed file under `self.dir`,
    /// deduplicating existing identical files and pruning oldest entries past retention bounds.
    pub fn try_spill(
        &self,
        kind_label: &str,
        full_content: &str,
    ) -> Result<String, SpillStoreError> {
        ensure_spill_dir(&self.dir)?;
        let clean_dir = canonicalize_clean(&self.dir);
        let file_name = format_content_addressed_spill_filename(kind_label, full_content);
        let final_path = clean_dir.join(&file_name);
        let expected_len = full_content.len() as u64;

        if let Ok(meta) = fs::metadata(&final_path)
            && meta.is_file()
            && meta.len() == expected_len
        {
            return Ok(format_portable_path(&final_path));
        }

        let temp_path = clean_dir.join(format!(".tmp-{file_name}-{}", Uuid::new_v4().simple()));
        let mut temp_guard = TempFileCleanupGuard::new(temp_path.clone());

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let mut file = opts
            .open(&temp_path)
            .map_err(|source| SpillStoreError::CreateTemp {
                path: temp_path.clone(),
                source,
            })?;
        file.write_all(full_content.as_bytes())
            .map_err(|source| SpillStoreError::WriteTemp {
                path: temp_path.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| SpillStoreError::WriteTemp {
                path: temp_path.clone(),
                source,
            })?;
        drop(file);

        if let Err(rename_err) = fs::rename(&temp_path, &final_path) {
            // On Windows, a concurrent writer persisting the identical content-addressed file
            // can win the rename race; verify length before treating as an error.
            let raced_identical = fs::metadata(&final_path)
                .is_ok_and(|meta| meta.is_file() && meta.len() == expected_len);
            if !raced_identical {
                return Err(SpillStoreError::Persist {
                    path: final_path,
                    source: rename_err,
                });
            }
        } else {
            temp_guard.disarm();
        }
        drop(temp_guard);

        prune_excess_spill_files(
            &clean_dir,
            &file_name,
            expected_len,
            self.max_files,
            self.max_total_bytes,
        );

        Ok(format_portable_path(&final_path))
    }

    /// Reads a bounded line/byte window or case-insensitive `query` filter from a persisted spill file.
    ///
    /// Streams through arbitrarily large spill files (`> 16 MiB`) with `O(1)` skipped-line memory,
    /// supports `byte_offset` continuation within overlong single lines (e.g. minified JSON), and
    /// strictly validates only the basename (`<slug>-<hash>.txt` or `.json`) inside `self.dir`.
    pub fn read_slice(
        &self,
        raw_ref: &str,
        start_line: usize,
        byte_offset: usize,
        max_lines: usize,
        query: Option<&str>,
    ) -> Result<String, SpillReadError> {
        const MAX_SPILL_SCAN_BYTES: u64 = 512 * 1024 * 1024;
        const MAX_SINGLE_LINE_BUFFER_BYTES: usize = 64 * 1024 * 1024;
        const MAX_SPILL_SLICE_OUTPUT_BYTES: usize = 9_000;
        const MAX_SPILL_SLICE_LINES: usize = 200;

        let file_name = extract_safe_spill_basename(raw_ref)?;
        let target_path = canonicalize_clean(&self.dir).join(file_name);
        let file = fs::File::open(&target_path).map_err(|source| SpillReadError::Open {
            file_name: file_name.to_owned(),
            source,
        })?;
        let mut reader = io::BufReader::with_capacity(64 * 1024, file);
        let mut remaining_scan_bytes = MAX_SPILL_SCAN_BYTES;

        let start_ln = start_line.max(1);
        let limit = max_lines.clamp(1, MAX_SPILL_SLICE_LINES);
        let clean_query = query.map(str::trim).filter(|q| !q.is_empty());

        let (skipped_lines, hit_eof_while_skipping) =
            skip_lines_before(&mut reader, start_ln, &mut remaining_scan_bytes).map_err(
                |source| SpillReadError::Read {
                    file_name: file_name.to_owned(),
                    source,
                },
            )?;

        if hit_eof_while_skipping {
            if let Some(needle) = clean_query {
                return Ok(format!(
                    "[Context spill `{file_name}` ({skipped_lines} lines) · 0 matches for `{needle}` from line {start_ln}]"
                ));
            }
            return Ok(format!(
                "[Context spill `{file_name}` ({skipped_lines} lines) · requested start_line={start_ln} is past EOF]"
            ));
        }

        let mut line_buf: Vec<u8> = Vec::with_capacity(1024);
        let mut body = String::with_capacity(2048);
        let mut emitted = 0usize;
        let mut first_ln = 0usize;
        let mut last_ln = 0usize;
        let mut line_no = skipped_lines;

        if let Some(needle) = clean_query {
            let needle_unicode_lower = (!needle.is_ascii()).then(|| needle.to_lowercase());
            let mut chunk_lower_buf = String::new();
            let mut total_matches = 0usize;
            while remaining_scan_bytes > 0
                && read_next_spill_line(
                    &mut reader,
                    &mut line_buf,
                    MAX_SINGLE_LINE_BUFFER_BYTES,
                    &mut remaining_scan_bytes,
                )
                .map_err(|source| SpillReadError::Read {
                    file_name: file_name.to_owned(),
                    source,
                })?
            {
                line_no = line_no.saturating_add(1);
                let line_cow = decode_line_lossless_or_lossy(&line_buf);
                let line = line_cow.as_ref();
                let search_start = if line_no == start_ln && byte_offset > 0 {
                    floor_utf8_boundary(line, byte_offset.min(line.len()))
                } else {
                    0
                };
                let haystack = line.get(search_start..).unwrap_or_default();
                let Some(rel_pos) = find_case_insensitive_byte_offset(
                    haystack,
                    needle,
                    needle_unicode_lower.as_deref(),
                    &mut chunk_lower_buf,
                ) else {
                    continue;
                };
                total_matches = total_matches.saturating_add(1);
                if emitted >= limit || body.len() >= MAX_SPILL_SLICE_OUTPUT_BYTES {
                    continue;
                }
                let abs_match_pos = search_start.saturating_add(rel_pos);
                let can_emit_full = search_start == 0
                    && line.len().saturating_add(24).saturating_add(body.len())
                        <= MAX_SPILL_SLICE_OUTPUT_BYTES;
                if can_emit_full {
                    if first_ln == 0 {
                        first_ln = line_no;
                    }
                    last_ln = line_no;
                    body.push_str(&format!("L{line_no}: {line}\n"));
                    emitted = emitted.saturating_add(1);
                } else if body.is_empty() {
                    if first_ln == 0 {
                        first_ln = line_no;
                    }
                    last_ln = line_no;
                    push_match_centered_overlong_row(
                        &mut body,
                        line_no,
                        line,
                        search_start,
                        abs_match_pos,
                        MAX_SPILL_SLICE_OUTPUT_BYTES,
                    );
                    emitted = emitted.saturating_add(1);
                }
            }
            let total_lines = line_no;
            if emitted == 0 {
                return Ok(format!(
                    "[Context spill `{file_name}` ({total_lines} lines) · 0 matches for `{needle}` from line {start_ln}]"
                ));
            }
            return Ok(format!(
                "[Context spill `{file_name}` ({total_lines} lines) · showing {emitted} of {total_matches} matches for `{needle}` (L{first_ln}..=L{last_ln})]\n{body}"
            ));
        }

        let mut capped_by_bytes = false;
        let mut overlong_line_continued = false;
        while remaining_scan_bytes > 0 && emitted < limit {
            let has_line = read_next_spill_line(
                &mut reader,
                &mut line_buf,
                MAX_SINGLE_LINE_BUFFER_BYTES,
                &mut remaining_scan_bytes,
            )
            .map_err(|source| SpillReadError::Read {
                file_name: file_name.to_owned(),
                source,
            })?;
            if !has_line {
                break;
            }
            line_no = line_no.saturating_add(1);
            let line_cow = decode_line_lossless_or_lossy(&line_buf);
            let line = line_cow.as_ref();

            if line_no == start_ln && byte_offset > 0 {
                if byte_offset >= line.len() {
                    let rest_lines = count_remaining_lines(&mut reader, &mut remaining_scan_bytes)
                        .map_err(|source| SpillReadError::Read {
                            file_name: file_name.to_owned(),
                            source,
                        })?;
                    let total_lines = line_no.saturating_add(rest_lines);
                    let line_bytes = line.len();
                    let next_ln = start_ln.saturating_add(1);
                    return Ok(format!(
                        "[Context spill `{file_name}` ({total_lines} lines) · requested byte_offset={byte_offset} is at/past end of L{start_ln} ({line_bytes} bytes); call with start_line={next_ln} and byte_offset=0]"
                    ));
                }
                let slice_start = floor_utf8_boundary(line, byte_offset);
                let tail = line.get(slice_start..).unwrap_or_default();
                let prefix = format!("L{line_no} [from byte_offset={slice_start}]: ");
                let avail = MAX_SPILL_SLICE_OUTPUT_BYTES.saturating_sub(prefix.len() + 240);
                first_ln = line_no;
                last_ln = line_no;
                emitted = emitted.saturating_add(1);
                if tail.len() > avail {
                    let cut = floor_utf8_boundary(tail, avail).max(1);
                    let safe_cut = floor_utf8_boundary(tail, cut.min(tail.len()));
                    let next_off = slice_start.saturating_add(safe_cut);
                    let line_total = line.len();
                    body.push_str(&prefix);
                    body.push_str(tail.get(..safe_cut).unwrap_or_default());
                    body.push_str(&format!(
                        "\n[... line L{line_no} continues at byte_offset={next_off} of {line_total} bytes; call mcp_lg_read_context_spill with start_line={line_no}, byte_offset={next_off} to continue ...]\n"
                    ));
                    overlong_line_continued = true;
                    break;
                }
                body.push_str(&prefix);
                body.push_str(tail);
                body.push('\n');
                continue;
            }

            let row_len = line.len().saturating_add(16);
            if !body.is_empty() && body.len().saturating_add(row_len) > MAX_SPILL_SLICE_OUTPUT_BYTES
            {
                // `line_no` was read from `reader` and belongs to the remaining lines after `last_ln`.
                let rest_lines = count_remaining_lines(&mut reader, &mut remaining_scan_bytes)
                    .map_err(|source| SpillReadError::Read {
                        file_name: file_name.to_owned(),
                        source,
                    })?;
                let total_lines = line_no.saturating_add(rest_lines);
                let next_ln = last_ln.saturating_add(1);
                body.push_str(&format!(
                    "[... slice capped at line {last_ln} to stay within lossless MCP window; call mcp_lg_read_context_spill with start_line={next_ln} to continue ...]\n"
                ));
                return Ok(format!(
                    "[Context spill `{file_name}` · lines {first_ln}..={last_ln} of {total_lines}]\n{body}"
                ));
            }

            if first_ln == 0 {
                first_ln = line_no;
            }
            last_ln = line_no;
            if row_len > MAX_SPILL_SLICE_OUTPUT_BYTES {
                let prefix = format!("L{line_no}: ");
                let avail = MAX_SPILL_SLICE_OUTPUT_BYTES.saturating_sub(prefix.len() + 240);
                let cut = floor_utf8_boundary(line, avail.min(line.len()));
                let line_total = line.len();
                body.push_str(&prefix);
                body.push_str(line.get(..cut).unwrap_or_default());
                body.push_str(&format!(
                    "\n[... line L{line_no} continues at byte_offset={cut} of {line_total} bytes; call mcp_lg_read_context_spill with start_line={line_no}, byte_offset={cut} to continue ...]\n"
                ));
                emitted = emitted.saturating_add(1);
                overlong_line_continued = true;
                break;
            }
            body.push_str(&format!("L{line_no}: {line}\n"));
            emitted = emitted.saturating_add(1);
            if body.len() >= MAX_SPILL_SLICE_OUTPUT_BYTES {
                capped_by_bytes = true;
                break;
            }
        }

        let rest_lines =
            count_remaining_lines(&mut reader, &mut remaining_scan_bytes).map_err(|source| {
                SpillReadError::Read {
                    file_name: file_name.to_owned(),
                    source,
                }
            })?;
        let total_lines = line_no.saturating_add(rest_lines);

        if emitted == 0 {
            return Ok(format!(
                "[Context spill `{file_name}` ({total_lines} lines) · requested start_line={start_ln} is past EOF]"
            ));
        }
        if capped_by_bytes && !overlong_line_continued && last_ln < total_lines {
            let next_ln = last_ln.saturating_add(1);
            body.push_str(&format!(
                "[... slice capped at line {last_ln} to stay within lossless MCP window; call mcp_lg_read_context_spill with start_line={next_ln} to continue ...]\n"
            ));
        }
        Ok(format!(
            "[Context spill `{file_name}` · lines {first_ln}..={last_ln} of {total_lines}]\n{body}"
        ))
    }
}

fn floor_utf8_boundary(s: &str, index: usize) -> usize {
    let mut pos = index.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Finds the first case-insensitive match of `needle` inside `haystack`, returning the exact
/// UTF-8 byte offset within `haystack` in `O(1)` auxiliary memory (`0` heap allocations for ASCII
/// needles; bounded `64 KiB` chunked scan with scalar offset translation for Unicode needles).
fn find_case_insensitive_byte_offset(
    haystack: &str,
    needle: &str,
    needle_unicode_lower: Option<&str>,
    chunk_lower_buf: &mut String,
) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.is_ascii() {
        let nb = needle.as_bytes();
        return haystack
            .as_bytes()
            .windows(nb.len())
            .position(|w| w.eq_ignore_ascii_case(nb));
    }
    let needle_lower = needle_unicode_lower?;
    if needle_lower.is_empty() {
        return Some(0);
    }
    let overlap = needle_lower.len().saturating_mul(4).saturating_add(16);
    let chunk_bytes = (64 * 1024).max(overlap.saturating_mul(4));
    let mut chunk_start = 0usize;
    while chunk_start < haystack.len() {
        let chunk_end = if chunk_start.saturating_add(chunk_bytes) >= haystack.len() {
            haystack.len()
        } else {
            let candidate = floor_utf8_boundary(haystack, chunk_start + chunk_bytes);
            if candidate > chunk_start {
                candidate
            } else {
                haystack.len()
            }
        };
        let chunk = haystack.get(chunk_start..chunk_end)?;
        chunk_lower_buf.clear();
        chunk_lower_buf.reserve(chunk.len().saturating_sub(chunk_lower_buf.capacity()));
        for ch in chunk.chars() {
            for lc in ch.to_lowercase() {
                chunk_lower_buf.push(lc);
            }
        }
        if let Some(target_lower_byte) = chunk_lower_buf.find(needle_lower) {
            let mut cur_lower = 0usize;
            let mut prev_orig = 0usize;
            for (rel_orig, ch) in chunk.char_indices() {
                if cur_lower == target_lower_byte {
                    return Some(chunk_start + rel_orig);
                }
                if cur_lower > target_lower_byte {
                    return Some(chunk_start + prev_orig);
                }
                prev_orig = rel_orig;
                cur_lower = cur_lower.saturating_add(ch.to_lowercase().map(char::len_utf8).sum());
            }
            return Some(chunk_start + prev_orig);
        }
        if chunk_end >= haystack.len() {
            break;
        }
        let next_start = floor_utf8_boundary(haystack, chunk_end.saturating_sub(overlap));
        if next_start <= chunk_start {
            let step = chunk.chars().next().map_or(1, char::len_utf8);
            chunk_start = chunk_start.saturating_add(step);
        } else {
            chunk_start = next_start;
        }
    }
    None
}

fn decode_line_lossless_or_lossy(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(valid) => std::borrow::Cow::Borrowed(valid),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

fn push_match_centered_overlong_row(
    body: &mut String,
    line_no: usize,
    line: &str,
    min_start: usize,
    match_pos: usize,
    max_bytes: usize,
) {
    let window_budget = max_bytes.saturating_sub(300).max(256);
    let raw_start = match_pos.saturating_sub(window_budget / 3).max(min_start);
    let win_start = floor_utf8_boundary(line, raw_start);
    let win_end = floor_utf8_boundary(
        line,
        win_start.saturating_add(window_budget).min(line.len()),
    );
    let line_total = line.len();
    let lead_ellipsis = if win_start > 0 { "..." } else { "" };
    let tail_ellipsis = if win_end < line_total { "..." } else { "" };
    let snippet = line.get(win_start..win_end).unwrap_or_default();
    body.push_str(&format!(
        "L{line_no} [bytes {win_start}..{win_end} of {line_total}]: {lead_ellipsis}{snippet}{tail_ellipsis}\n"
    ));
    if win_end < line_total {
        body.push_str(&format!(
            "[... match line L{line_no} continues at byte_offset={win_end} of {line_total} bytes; call mcp_lg_read_context_spill with start_line={line_no}, byte_offset={win_end} to continue ...]\n"
        ));
    }
}

/// Skips `start_line - 1` lines in `O(1)` memory without allocating strings for skipped lines.
/// Returns `(completed_lines_before_target, hit_eof)`.
fn skip_lines_before<R: io::BufRead>(
    reader: &mut R,
    start_line: usize,
    remaining_scan_bytes: &mut u64,
) -> io::Result<(usize, bool)> {
    let target_to_skip = start_line.saturating_sub(1);
    if target_to_skip == 0 {
        let peek = reader.fill_buf()?;
        return Ok((0, peek.is_empty()));
    }
    let mut skipped = 0usize;
    let mut in_unterminated_line = false;
    while skipped < target_to_skip && *remaining_scan_bytes > 0 {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if in_unterminated_line {
                skipped = skipped.saturating_add(1);
            }
            return Ok((skipped, true));
        }
        let allowed =
            usize::try_from((*remaining_scan_bytes).min(buf.len() as u64)).unwrap_or(buf.len());
        let window = buf.get(..allowed).unwrap_or(buf);
        if let Some(nl_pos) = window.iter().position(|&b| b == b'\n') {
            let consume_len = nl_pos + 1;
            reader.consume(consume_len);
            *remaining_scan_bytes = remaining_scan_bytes.saturating_sub(consume_len as u64);
            skipped = skipped.saturating_add(1);
            in_unterminated_line = false;
        } else {
            let consume_len = window.len();
            reader.consume(consume_len);
            *remaining_scan_bytes = remaining_scan_bytes.saturating_sub(consume_len as u64);
            in_unterminated_line = true;
        }
    }
    let peek = reader.fill_buf()?;
    Ok((skipped, peek.is_empty()))
}

/// Reads the next `\n`-delimited (or EOF-terminated) line into `out`, stripping trailing `\r\n` / `\n`.
fn read_next_spill_line<R: io::BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_line_bytes: usize,
    remaining_scan_bytes: &mut u64,
) -> io::Result<bool> {
    out.clear();
    let mut read_any = false;
    while *remaining_scan_bytes > 0 {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let allowed =
            usize::try_from((*remaining_scan_bytes).min(buf.len() as u64)).unwrap_or(buf.len());
        let window = buf.get(..allowed).unwrap_or(buf);
        read_any = true;
        if let Some(nl_pos) = window.iter().position(|&b| b == b'\n') {
            let line_part = window.get(..nl_pos).unwrap_or_default();
            if out.len() < max_line_bytes {
                let take = (max_line_bytes - out.len()).min(line_part.len());
                out.extend_from_slice(line_part.get(..take).unwrap_or_default());
            }
            let consume_len = nl_pos + 1;
            reader.consume(consume_len);
            *remaining_scan_bytes = remaining_scan_bytes.saturating_sub(consume_len as u64);
            if out.last() == Some(&b'\r') {
                let _ = out.pop();
            }
            return Ok(true);
        }
        if out.len() < max_line_bytes {
            let take = (max_line_bytes - out.len()).min(window.len());
            out.extend_from_slice(window.get(..take).unwrap_or_default());
        }
        let consume_len = window.len();
        reader.consume(consume_len);
        *remaining_scan_bytes = remaining_scan_bytes.saturating_sub(consume_len as u64);
    }
    if read_any && out.last() == Some(&b'\r') {
        let _ = out.pop();
    }
    Ok(read_any)
}

/// Counts remaining lines in `reader` in `O(1)` memory, matching `str::lines().count()` semantics.
fn count_remaining_lines<R: io::BufRead>(
    reader: &mut R,
    remaining_scan_bytes: &mut u64,
) -> io::Result<usize> {
    let mut lines = 0usize;
    let mut in_unterminated_line = false;
    while *remaining_scan_bytes > 0 {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let allowed =
            usize::try_from((*remaining_scan_bytes).min(buf.len() as u64)).unwrap_or(buf.len());
        let window = buf.get(..allowed).unwrap_or(buf);
        for &b in window {
            if b == b'\n' {
                lines = lines.saturating_add(1);
                in_unterminated_line = false;
            } else {
                in_unterminated_line = true;
            }
        }
        let consume_len = window.len();
        reader.consume(consume_len);
        *remaining_scan_bytes = remaining_scan_bytes.saturating_sub(consume_len as u64);
    }
    if in_unterminated_line {
        lines = lines.saturating_add(1);
    }
    Ok(lines)
}

impl TruncationSpillSink for SpillStore {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        match self.try_spill(kind_label, full_content) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!(
                    error = ?err,
                    kind = kind_label,
                    bytes = full_content.len(),
                    "failed to persist full truncated payload to local spill directory"
                );
                None
            }
        }
    }
}

/// Structured failure when reading a gateway-local spill slice via `mcp_lg_read_context_spill`.
#[derive(Debug, Error)]
pub enum SpillReadError {
    #[error(
        "invalid spill file reference `{raw}` (expected a `.txt` or `.json` spill filename or path)"
    )]
    InvalidReference { raw: String },

    #[error("spill file `{file_name}` was not found (it may have rotated or expired)")]
    Open {
        file_name: String,
        #[source]
        source: io::Error,
    },

    #[error("failed reading spill file `{file_name}`")]
    Read {
        file_name: String,
        #[source]
        source: io::Error,
    },
}

fn extract_safe_spill_basename(raw_ref: &str) -> Result<&str, SpillReadError> {
    let trimmed = raw_ref.trim().trim_matches('`').trim_matches('"');
    let without_scheme = trimmed.strip_prefix("spill://").unwrap_or(trimmed);
    let last_seg = without_scheme
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let valid = !last_seg.is_empty()
        && last_seg.len() <= 160
        && !last_seg.starts_with('.')
        && !last_seg.contains("..")
        && (last_seg.ends_with(".txt") || last_seg.ends_with(".json"))
        && last_seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err(SpillReadError::InvalidReference {
            raw: raw_ref.to_owned(),
        });
    }
    Ok(last_seg)
}

fn ensure_spill_dir(dir: &Path) -> Result<(), SpillStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(dir)
            .map_err(|source| SpillStoreError::CreateDir {
                path: dir.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|source| SpillStoreError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })
    }
}

fn format_portable_path(path: &Path) -> String {
    canonicalize_clean(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn prune_excess_spill_files(
    clean_dir: &Path,
    protected_file_name: &str,
    protected_len: u64,
    max_files: usize,
    max_total_bytes: u64,
) {
    let Ok(read_dir) = fs::read_dir(clean_dir) else {
        return;
    };
    let mut candidates: Vec<(u128, u64, PathBuf)> = Vec::new();
    let mut total_bytes = protected_len;
    let mut total_files = 1usize;

    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let os_name = entry.file_name();
        let Some(name) = os_name.to_str() else {
            continue;
        };
        if name.starts_with(".tmp-") || name == protected_file_name {
            continue;
        }
        if !(name.ends_with(".txt") || name.ends_with(".json")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let len = meta.len();
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        total_bytes = total_bytes.saturating_add(len);
        total_files = total_files.saturating_add(1);
        candidates.push((modified_ns, len, entry.path()));
    }

    if total_files <= max_files && total_bytes <= max_total_bytes {
        return;
    }

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    for (_, len, path) in candidates {
        if total_files <= max_files && total_bytes <= max_total_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total_files = total_files.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_content_addressed_spills_and_prunes_oldest_entries() {
        let dir = std::env::temp_dir().join(format!("liftgate-spill-test-{}", Uuid::new_v4()));
        let store = SpillStore::with_limits(&dir, 3, 20_000);

        let p1 = store.try_spill("tool output", "alpha payload").unwrap();
        let p1_dup = store.try_spill("tool output", "alpha payload").unwrap();
        assert_eq!(p1, p1_dup);
        assert!(p1.ends_with(".txt"));
        assert!(!p1.contains('\\'));
        assert_eq!(fs::read_to_string(&p1).unwrap(), "alpha payload");

        let json_path = store
            .try_spill("client tools catalog", r#"[{"name":"Bash"}]"#)
            .unwrap();
        assert!(json_path.ends_with(".json"));

        let _ = store.try_spill("input text", &"b".repeat(4_000)).unwrap();
        let newest = store.try_spill("input text", &"c".repeat(4_000)).unwrap();

        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(
            remaining.len() <= 3,
            "expected bounded retention <= 3 files, got {}",
            remaining.len()
        );
        assert_eq!(fs::read_to_string(&newest).unwrap(), "c".repeat(4_000));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_slice_supports_txt_and_json_spills_queries_byte_caps_and_rejects_traversal() {
        let dir = std::env::temp_dir().join(format!("liftgate-spill-slice-{}", Uuid::new_v4()));
        let store = SpillStore::new(&dir);

        let lines_content = (1..=300)
            .map(|i| format!("row_{i:03}: exact source line content for verification"))
            .collect::<Vec<_>>()
            .join("\n");
        let txt_path = store
            .try_spill("tool_result:exec_command", &lines_content)
            .unwrap();
        let slice = store.read_slice(&txt_path, 140, 0, 5, None).unwrap();
        assert!(slice.contains("lines 140..=144 of 300"));
        assert!(slice.contains("L140: row_140:"));
        assert!(slice.contains("L144: row_144:"));
        assert!(!slice.contains("L145:"));

        let filtered = store
            .read_slice(&txt_path, 1, 0, 20, Some("row_255"))
            .unwrap();
        assert!(filtered.contains("showing 1 of 1 matches for `row_255`"));
        assert!(filtered.contains("L255: row_255:"));

        // `.json` spill files (including minified >22KB single-line JSON) support full `byte_offset`
        // continuation AND match-centered `query` windows while staying strictly < 10KB per slice!
        let minified_json = format!(
            r#"{{"prefix":"{}","needle_key":"TAIL_SECRET_42","suffix":"{}"}}"#,
            "x".repeat(14_000),
            "y".repeat(9_000)
        );
        let json_path = store
            .try_spill("client tools catalog", &minified_json)
            .unwrap();
        assert!(json_path.ends_with(".json"));
        let json_slice_1 = store.read_slice(&json_path, 1, 0, 50, None).unwrap();
        assert!(
            json_slice_1.len() < 10_000,
            "single overlong JSON line must stay strictly below agy acp 10KB truncation cliff"
        );
        assert!(json_slice_1.contains("continues at byte_offset="));
        let next_off_1: usize = json_slice_1
            .split("byte_offset=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("must emit parseable byte_offset");
        let json_slice_2 = store
            .read_slice(&json_path, 1, next_off_1, 50, None)
            .unwrap();
        assert!(json_slice_2.len() < 10_000);
        assert!(
            json_slice_2.contains("TAIL_SECRET_42"),
            "byte_offset continuation must recover middle/tail of overlong JSON line:\n{json_slice_2}"
        );

        let query_overlong = store
            .read_slice(&json_path, 1, 0, 10, Some("TAIL_SECRET_42"))
            .unwrap();
        assert!(query_overlong.len() < 10_000);
        assert!(
            query_overlong.contains("TAIL_SECRET_42"),
            "query on overlong line must center window around the matching region:\n{query_overlong}"
        );

        // Non-ASCII Unicode case-insensitive search (`привет` matching `ПРИВЕТ` at byte >14,000)
        // must also center the snippet around the actual match offset rather than byte 0!
        let unicode_overlong = format!(
            r#"{{"pad":"{}","greeting":"ПРИВЕТ_МИР_77","tail":"{}"}}"#,
            "a".repeat(150_000),
            "b".repeat(4_000)
        );
        let unicode_path = store
            .try_spill("client tools catalog", &unicode_overlong)
            .unwrap();
        let unicode_query = store
            .read_slice(&unicode_path, 1, 0, 5, Some("привет_мир_77"))
            .unwrap();
        assert!(unicode_query.len() < 10_000);
        assert!(
            unicode_query.contains("ПРИВЕТ_МИР_77"),
            "Unicode case-insensitive query must preserve exact match offset on overlong lines:\n{unicode_query}"
        );

        // Multi-line byte cap emits exact next `start_line` continuation hint
        let wide_lines = (1..=150)
            .map(|i| format!("L_{i:03}_{}", "a".repeat(120)))
            .collect::<Vec<_>>()
            .join("\n");
        let wide_path = store.try_spill("tool output", &wide_lines).unwrap();
        let capped = store.read_slice(&wide_path, 1, 0, 200, None).unwrap();
        assert!(capped.len() < 10_000);
        assert!(capped.contains("call mcp_lg_read_context_spill with start_line="));

        // Spill files larger than 16 MiB (with multi-byte UTF-8 crossing the 16 MiB mark) stream cleanly!
        let utf8_line = format!("日志条目-αβγδε-{}\n", "z".repeat(960));
        let line_count = (17 * 1024 * 1024) / utf8_line.len() + 25;
        let mut huge_payload = String::with_capacity(line_count * utf8_line.len() + 128);
        for idx in 1..line_count {
            huge_payload.push_str(&utf8_line);
            if idx == line_count - 3 {
                huge_payload.push_str("BEYOND_16_MIB_MARKER_LINE: success\n");
            }
        }
        assert!(huge_payload.len() > 17 * 1024 * 1024);
        let huge_path = store.try_spill("tool output", &huge_payload).unwrap();
        let marker_line_no = line_count - 2;
        let beyond_slice = store
            .read_slice(&huge_path, marker_line_no, 0, 3, None)
            .unwrap();
        assert!(
            beyond_slice.contains("BEYOND_16_MIB_MARKER_LINE: success"),
            "must read lines past 16 MiB without prefix cutoff or UTF-8 split failure:\n{beyond_slice}"
        );
        let beyond_query = store
            .read_slice(&huge_path, 1, 0, 5, Some("BEYOND_16_MIB_MARKER_LINE"))
            .unwrap();
        assert!(beyond_query.contains("BEYOND_16_MIB_MARKER_LINE: success"));

        // Path traversal / non-spill extensions are rejected
        assert!(store.read_slice("../etc/passwd", 1, 0, 10, None).is_err());
        assert!(store.read_slice(".hidden.txt", 1, 0, 10, None).is_err());
        assert!(store.read_slice("payload.exe", 1, 0, 10, None).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
