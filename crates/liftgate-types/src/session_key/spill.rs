//! Lossless spill persistence, UTF-8/line truncation, and terminal control noise scrubbing.

use sha2::{Digest, Sha256};

/// Pluggable sink that persists the complete, un-truncated UTF-8 payload to a local file
/// before any head+tail truncation, schema compaction, or archival turn omission is applied.
///
/// Implementations return the normalized local filesystem path so inline omission markers
/// can tell client agents (`Read`, `Grep`, `rg`, shell tools) exactly where the lossless
/// content is stored on disk.
pub trait TruncationSpillSink {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String>;
}

/// No-op [`TruncationSpillSink`] used when local spill persistence is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSpillSink;

impl TruncationSpillSink for NoopSpillSink {
    fn spill_truncated_content(
        &mut self,
        _kind_label: &str,
        _full_content: &str,
    ) -> Option<String> {
        None
    }
}

impl<S: TruncationSpillSink + ?Sized> TruncationSpillSink for &mut S {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        (**self).spill_truncated_content(kind_label, full_content)
    }
}

impl<S: TruncationSpillSink> TruncationSpillSink for Option<S> {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        self.as_mut()
            .and_then(|inner| inner.spill_truncated_content(kind_label, full_content))
    }
}

/// Formats a deterministic, content-addressed spill filename (`<slug>-<sha256_16>.<ext>`)
/// so identical payloads across turns or retries deduplicate cleanly and preserve prompt
/// determinism for upstream KV prefix caching.
#[must_use]
pub fn format_content_addressed_spill_filename(kind_label: &str, full_content: &str) -> String {
    let mut slug = String::with_capacity(kind_label.len().min(40));
    let mut prev_dash = false;
    for ch in kind_label.chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        let _ = slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("payload");
    }

    let digest = Sha256::digest(full_content.as_bytes());
    let mut hash_hex = String::with_capacity(16);
    for &byte in digest.get(..8).unwrap_or(&[]) {
        use std::fmt::Write as _;
        let _ = write!(&mut hash_hex, "{byte:02x}");
    }

    let trimmed = full_content.trim();
    let ext = if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_ok()
    {
        "json"
    } else {
        "txt"
    };

    format!("{slug}-{hash_hex}.{ext}")
}

/// In-memory [`TruncationSpillSink`] for deterministic unit and integration assertions.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Default)]
pub struct MemorySpillSink {
    base_dir: String,
    files: std::collections::BTreeMap<String, String>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemorySpillSink {
    #[must_use]
    pub fn new(base_dir: impl Into<String>) -> Self {
        let mut dir = base_dir.into().replace('\\', "/");
        while dir.ends_with('/') {
            let _ = dir.pop();
        }
        Self {
            base_dir: dir,
            files: std::collections::BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn files(&self) -> &std::collections::BTreeMap<String, String> {
        &self.files
    }
}

#[cfg(any(test, feature = "test-support"))]
impl TruncationSpillSink for MemorySpillSink {
    fn spill_truncated_content(&mut self, kind_label: &str, full_content: &str) -> Option<String> {
        let file_name = format_content_addressed_spill_filename(kind_label, full_content);
        let full_path = format!("{}/{file_name}", self.base_dir);
        self.files
            .insert(full_path.clone(), full_content.to_owned());
        Some(full_path)
    }
}

pub(super) fn compact_middle_chars(input: &str, max_chars: usize) -> (String, bool) {
    let total_chars = input.chars().count();
    if total_chars <= max_chars {
        return (input.to_owned(), false);
    }
    const MARKER: &str = " ... ";
    let marker_chars = 5usize;
    if max_chars <= marker_chars + 4 {
        let head: String = input.chars().take(max_chars.saturating_sub(3)).collect();
        return (format!("{head}..."), true);
    }
    let usable = max_chars.saturating_sub(marker_chars);
    let head_chars = (usable * 3) / 5;
    let tail_chars = usable.saturating_sub(head_chars);
    let head: String = input.chars().take(head_chars).collect();
    let tail_skip = total_chars.saturating_sub(tail_chars);
    let tail: String = input.chars().skip(tail_skip).collect();
    (
        format!("{}{MARKER}{}", head.trim_end(), tail.trim_start()),
        true,
    )
}

pub(super) fn floor_utf8_boundary(s: &str, max_byte: usize) -> usize {
    let mut idx = max_byte.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub(super) fn ceil_utf8_boundary(s: &str, min_byte: usize) -> usize {
    let mut idx = min_byte.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub(super) fn count_logical_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.bytes().filter(|&b| b == b'\n').count() + usize::from(!s.ends_with('\n'))
    }
}

/// Truncates an oversized UTF-8 payload in the middle while preserving a generous head (~55%)
/// and tail (~45%) snapped cleanly to line boundaries, inserting a structured diagnostic marker
/// with exact byte and 1-based line ranges so coding agents understand that the middle was
/// omitted and can inspect a targeted range if needed.
#[must_use]
pub fn truncate_head_tail_lines(input: &str, max_bytes: usize, kind_label: &str) -> String {
    truncate_head_tail_lines_with_spill(input, max_bytes, kind_label, &mut NoopSpillSink)
}

/// Like [`truncate_head_tail_lines`], persisting the full un-truncated `input` to a local file
/// via `spill` and referencing its path directly inside the middle omission notice.
#[must_use]
pub fn truncate_head_tail_lines_with_spill<S: TruncationSpillSink>(
    input: &str,
    max_bytes: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    truncate_head_tail_lines_from_source(input, input, None, max_bytes, kind_label, spill)
}

/// Strips terminal ANSI escape sequences (`CSI`, `OSC`, `DCS`, 2-byte `ESC`), backspace
/// overstrikes, and line-local carriage-return (`\r`) progress-bar overwrites while preserving
/// `\r\n` / `\r\r\n` line endings and UTF-8 integrity. Returns `Cow::Borrowed` with zero
/// allocation when no terminal control noise is present.
#[must_use]
pub fn scrub_terminal_control_noise(input: &str) -> std::borrow::Cow<'_, str> {
    if !has_terminal_control_noise(input) {
        return std::borrow::Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut line_start = 0usize;
    let mut pending_cr = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    let mut csi_param = String::new();
                    while let Some(&b) = chars.peek() {
                        if ('\x20'..='\x3f').contains(&b) {
                            if csi_param.len() < 8 {
                                csi_param.push(b);
                            }
                            let _ = chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(&final_ch) = chars.peek()
                        && ('\x40'..='\x7e').contains(&final_ch)
                    {
                        let _ = chars.next();
                        if final_ch == 'K'
                            && (csi_param == "2"
                                || (pending_cr && (csi_param.is_empty() || csi_param == "0")))
                        {
                            out.truncate(line_start);
                            pending_cr = false;
                        }
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    while let Some(osc_ch) = chars.next() {
                        if osc_ch == '\x07' {
                            break;
                        }
                        if osc_ch == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some('P' | 'X' | '^' | '_') => {
                    let _ = chars.next();
                    while let Some(st_ch) = chars.next() {
                        if st_ch == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    let _ = chars.next();
                    let _ = chars.next();
                }
                Some(esc_next) if ('\x40'..='\x5f').contains(&esc_next) => {
                    let _ = chars.next();
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                    pending_cr = false;
                    out.push('\n');
                    line_start = out.len();
                } else {
                    pending_cr = true;
                }
            }
            '\n' => {
                pending_cr = false;
                out.push('\n');
                line_start = out.len();
            }
            '\x08' => {
                if !pending_cr && out.len() > line_start {
                    let _ = out.pop();
                }
            }
            other => {
                if pending_cr {
                    out.truncate(line_start);
                    pending_cr = false;
                }
                out.push(other);
            }
        }
    }

    std::borrow::Cow::Owned(out)
}

fn has_terminal_control_noise(input: &str) -> bool {
    let mut iter = input.as_bytes().iter().copied().peekable();
    while let Some(b) = iter.next() {
        if b == 0x1b || b == 0x08 {
            return true;
        }
        if b == b'\r' && iter.peek().copied() != Some(b'\n') {
            return true;
        }
    }
    false
}

fn contains_ascii_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    let n_bytes = needle_lower.as_bytes();
    if n_bytes.is_empty() {
        return true;
    }
    let h_bytes = haystack.as_bytes();
    if h_bytes.len() < n_bytes.len() {
        return false;
    }
    h_bytes
        .windows(n_bytes.len())
        .any(|win| win.eq_ignore_ascii_case(n_bytes))
}

fn is_diagnostic_noise_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if contains_ascii_case_insensitive(trimmed, "[error] to see the full stack trace")
        || contains_ascii_case_insensitive(trimmed, "[error] re-run maven")
        || contains_ascii_case_insensitive(trimmed, "[error] for more information")
        || contains_ascii_case_insensitive(trimmed, "-> [help 1]")
    {
        return true;
    }
    let is_json_info = trimmed.starts_with('{')
        && (contains_ascii_case_insensitive(trimmed, "\"level\":10")
            || contains_ascii_case_insensitive(trimmed, "\"level\":20")
            || contains_ascii_case_insensitive(trimmed, "\"level\":30")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"info\"")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"debug\"")
            || contains_ascii_case_insensitive(trimmed, "\"level\":\"trace\""));
    if is_json_info {
        return !contains_ascii_case_insensitive(trimmed, "error")
            && !contains_ascii_case_insensitive(trimmed, "exception")
            && !contains_ascii_case_insensitive(trimmed, "fatal");
    }
    false
}

fn classify_diagnostic_context_lines(line: &str) -> Option<usize> {
    if is_diagnostic_noise_line(line) {
        return None;
    }
    let trimmed = line.trim();
    const ROOT_CAUSE_MARKERS: &[&str] = &[
        "caused by:",
        "panicked at",
        "application failed to start",
        "compilation error",
        "traceback (most recent call last)",
        "failures:",
        "failed tests",
    ];
    if ROOT_CAUSE_MARKERS
        .iter()
        .any(|m| contains_ascii_case_insensitive(trimmed, m))
    {
        return Some(5);
    }

    const ERROR_MARKERS: &[&str] = &[
        "error:",
        "error[",
        "fatal:",
        "fatal error:",
        "npm err!",
        "--- fail:",
        "[  failed  ]",
        "build failed",
        "compilation failed",
        "undefined reference to",
        "assertion failed",
        "assertionerror",
        "syntaxerror:",
        "typeerror:",
        "referenceerror:",
    ];
    if ERROR_MARKERS
        .iter()
        .any(|m| contains_ascii_case_insensitive(trimmed, m))
    {
        return Some(3);
    }
    if (contains_ascii_case_insensitive(trimmed, "test ")
        && trimmed
            .get(trimmed.len().saturating_sub(10)..)
            .is_some_and(|tail| contains_ascii_case_insensitive(tail, "... failed")))
        || trimmed
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("failed "))
    {
        return Some(3);
    }
    None
}

/// Scans an omitted middle slice of a tool output for compiler/test/runtime failure root causes,
/// returning a bounded line-numbered excerpt when high-signal diagnostics would otherwise be lost.
fn extract_omitted_diagnostic_excerpt(
    omitted_slice: &str,
    first_omitted_line: usize,
    max_excerpt_bytes: usize,
) -> Option<String> {
    if max_excerpt_bytes < 160 {
        return None;
    }
    let lines: Vec<&str> = omitted_slice.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut keep = vec![false; lines.len()];
    let mut matched_any = false;
    for (idx, &line) in lines.iter().enumerate() {
        if let Some(after) = classify_diagnostic_context_lines(line) {
            matched_any = true;
            let from = idx.saturating_sub(1);
            let to = (idx + after).min(lines.len().saturating_sub(1));
            for slot in keep.get_mut(from..=to).into_iter().flatten() {
                *slot = true;
            }
        }
    }
    if !matched_any {
        return None;
    }

    let mut out = String::with_capacity(max_excerpt_bytes.min(1_024));
    let mut prev_idx: Option<usize> = None;
    for (idx, (&is_kept, &raw_line)) in keep.iter().zip(lines.iter()).enumerate() {
        if !is_kept || is_diagnostic_noise_line(raw_line) {
            continue;
        }
        if let Some(prev) = prev_idx
            && idx > prev + 1
        {
            let gap = "  ...\n";
            if out.len() + gap.len() > max_excerpt_bytes {
                break;
            }
            out.push_str(gap);
        }
        let line_no = first_omitted_line + idx;
        let trimmed = raw_line.trim_end();
        let clamped = if trimmed.len() > 260 {
            let cut = floor_utf8_boundary(trimmed, 257);
            format!("{}...", trimmed.get(..cut).unwrap_or(""))
        } else {
            trimmed.to_owned()
        };
        let formatted = format!("  L{line_no}: {clamped}\n");
        if out.len() + formatted.len() > max_excerpt_bytes {
            break;
        }
        out.push_str(&formatted);
        prev_idx = Some(idx);
    }

    if out.is_empty() { None } else { Some(out) }
}

fn slice_head_tail_on_lines(input: &str, usable: usize) -> (&str, &str, &str) {
    let total_bytes = input.len();
    let head_budget = usable.saturating_mul(11) / 20;
    let tail_budget = usable.saturating_sub(head_budget);

    let raw_head_end = floor_utf8_boundary(input, head_budget);
    let min_head_snap = (head_budget * 4) / 5;
    let head_end = input
        .get(min_head_snap..raw_head_end)
        .and_then(|win| win.rfind('\n').map(|pos| min_head_snap + pos))
        .unwrap_or(raw_head_end);

    let raw_tail_start =
        ceil_utf8_boundary(input, total_bytes.saturating_sub(tail_budget)).max(head_end);
    let max_tail_snap = total_bytes
        .saturating_sub((tail_budget * 4) / 5)
        .max(raw_tail_start);
    let tail_start = input
        .get(raw_tail_start..max_tail_snap)
        .and_then(|win| win.find('\n').map(|pos| raw_tail_start + pos + 1))
        .unwrap_or(raw_tail_start)
        .max(head_end);

    let head = input
        .get(..head_end)
        .unwrap_or("")
        .trim_end_matches(['\r', '\n']);
    let tail = input
        .get(tail_start..)
        .unwrap_or("")
        .trim_start_matches(['\r', '\n']);
    let omitted_end = total_bytes.saturating_sub(tail.len()).max(head.len());
    let omitted_slice = input.get(head.len()..omitted_end).unwrap_or("");
    (head, omitted_slice, tail)
}

pub(super) fn truncate_head_tail_lines_from_source<S: TruncationSpillSink>(
    input: &str,
    full_source_for_spill: &str,
    pre_spilled_path: Option<&str>,
    max_bytes: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let spilled_path = pre_spilled_path
        .map(str::to_owned)
        .or_else(|| spill.spill_truncated_content(kind_label, full_source_for_spill));

    let path_extra = spilled_path.as_deref().map_or(0, |p| p.len() * 2 + 160);
    let marker_reserve = 380usize.saturating_add(path_extra);
    if max_bytes <= marker_reserve.saturating_add(64) {
        return truncate_middle_utf8_bytes_pre_spilled(
            input,
            max_bytes,
            11,
            20,
            spilled_path.as_deref(),
        );
    }

    let total_bytes = input.len();
    let usable = max_bytes.saturating_sub(marker_reserve);
    let (mut head, mut omitted_slice, mut tail) = slice_head_tail_on_lines(input, usable);

    // For oversized tool outputs, rescue buried compiler/test failure lines from the omitted
    // middle so the model sees root-cause diagnostics inline without an extra file-read turn.
    let mut diag_excerpt: Option<String> = None;
    if matches!(kind_label, "tool output" | "archival tool output")
        && usable >= 2_400
        && omitted_slice.contains('\n')
    {
        let probe_first_line = if head.is_empty() {
            1
        } else {
            head.bytes().filter(|&b| b == b'\n').count() + 2
        };
        let diag_cap = (usable / 5).min(4_096);
        if extract_omitted_diagnostic_excerpt(omitted_slice, probe_first_line, diag_cap).is_some() {
            let shrunk_usable = usable.saturating_sub(diag_cap.saturating_add(96));
            let (h2, om2, t2) = slice_head_tail_on_lines(input, shrunk_usable);
            let first_line_2 = if h2.is_empty() {
                1
            } else {
                h2.bytes().filter(|&b| b == b'\n').count() + 2
            };
            if let Some(excerpt) = extract_omitted_diagnostic_excerpt(om2, first_line_2, diag_cap) {
                head = h2;
                omitted_slice = om2;
                tail = t2;
                diag_excerpt = Some(excerpt);
            }
        }
    }

    let omitted_bytes = omitted_slice.len();

    let mut notice = if omitted_slice.contains('\n') {
        let total_lines = count_logical_lines(input);
        let head_lines = if head.is_empty() {
            0
        } else {
            head.bytes().filter(|&b| b == b'\n').count() + 1
        };
        let tail_lines = count_logical_lines(tail);
        let first_omitted = head_lines + 1;
        let last_omitted = total_lines.saturating_sub(tail_lines).max(first_omitted);
        let omitted_lines = last_omitted.saturating_sub(first_omitted) + 1;
        if let Some(path) = &spilled_path {
            format!(
                "\n... [Gateway truncated {kind_label}: {total_bytes} bytes ({total_lines} lines) total; omitted middle {omitted_bytes} bytes ({omitted_lines} lines, lines {first_omitted}..={last_omitted}) to fit single-turn limit. Full lossless content saved locally at `{path}` — beginning and end are preserved intact; inspect `{path}` (e.g. lines {first_omitted}..={last_omitted}) if the omitted middle section is needed.] ...\n"
            )
        } else {
            format!(
                "\n... [Gateway truncated {kind_label}: {total_bytes} bytes ({total_lines} lines) total; omitted middle {omitted_bytes} bytes ({omitted_lines} lines, lines {first_omitted}..={last_omitted}) to fit single-turn limit. Beginning and end are preserved intact; query that specific line range or filter output if the middle section is needed.] ...\n"
            )
        }
    } else if let Some(path) = &spilled_path {
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes total; omitted middle {omitted_bytes} bytes to fit single-turn limit. Full lossless content saved locally at `{path}` — beginning and end are preserved intact; inspect `{path}` if the omitted middle section is needed.] ...\n"
        )
    } else {
        format!(
            "\n... [Gateway truncated {kind_label}: {total_bytes} bytes total; omitted middle {omitted_bytes} bytes to fit single-turn limit. Beginning and end are preserved intact.] ...\n"
        )
    };
    if let Some(excerpt) = diag_excerpt {
        notice.push_str("[Diagnostics extracted from omitted middle:]\n");
        notice.push_str(&excerpt);
        notice.push_str("[End extracted diagnostics]\n");
    }

    let mut out = String::with_capacity(head.len() + notice.len() + tail.len());
    out.push_str(head);
    out.push_str(&notice);
    out.push_str(tail);
    if out.len() > max_bytes {
        truncate_middle_utf8_bytes_pre_spilled(input, max_bytes, 11, 20, spilled_path.as_deref())
    } else {
        out
    }
}

pub(super) fn truncate_middle_utf8_bytes_with_spill<S: TruncationSpillSink>(
    input: &str,
    max_bytes: usize,
    head_num: usize,
    denom: usize,
    kind_label: &str,
    spill: &mut S,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let spilled = spill.spill_truncated_content(kind_label, input);
    truncate_middle_utf8_bytes_pre_spilled(input, max_bytes, head_num, denom, spilled.as_deref())
}

pub(super) fn truncate_middle_utf8_bytes_pre_spilled(
    input: &str,
    max_bytes: usize,
    head_num: usize,
    denom: usize,
    spilled_path: Option<&str>,
) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }

    if let Some(path) = spilled_path {
        let full_marker_overhead = path.len().saturating_add(74);
        if max_bytes >= full_marker_overhead.saturating_add(16) {
            let usable = max_bytes.saturating_sub(full_marker_overhead);
            let safe_denom = denom.max(1);
            let head_budget = usable.saturating_mul(head_num.min(safe_denom)) / safe_denom;
            let tail_budget = usable.saturating_sub(head_budget);

            let head_end = floor_utf8_boundary(input, head_budget);
            let tail_start =
                ceil_utf8_boundary(input, input.len().saturating_sub(tail_budget)).max(head_end);
            let head = input.get(..head_end).unwrap_or("").trim_end();
            let tail = input.get(tail_start..).unwrap_or("").trim_start();
            let omitted_bytes = tail_start.saturating_sub(head_end);
            let marker = format!(
                "\n...[omitted {omitted_bytes} bytes; full content saved at `{path}`]...\n"
            );
            let mut out = String::with_capacity(head.len() + marker.len() + tail.len());
            out.push_str(head);
            out.push_str(&marker);
            out.push_str(tail);
            return out;
        }
        let compact_notice = format!("...[omitted; full: `{path}`]");
        if max_bytes >= compact_notice.len() {
            let head_end =
                floor_utf8_boundary(input, max_bytes.saturating_sub(compact_notice.len()));
            let head = input.get(..head_end).unwrap_or("").trim_end();
            return format!("{head}{compact_notice}");
        }
    }

    if max_bytes <= 80 {
        let head_end = floor_utf8_boundary(input, max_bytes.saturating_sub(3));
        let head = input.get(..head_end).unwrap_or("");
        return format!("{head}...");
    }

    let marker_overhead = 56usize;
    let usable = max_bytes.saturating_sub(marker_overhead);
    let safe_denom = denom.max(1);
    let head_budget = usable.saturating_mul(head_num.min(safe_denom)) / safe_denom;
    let tail_budget = usable.saturating_sub(head_budget);

    let head_end = floor_utf8_boundary(input, head_budget);
    let tail_start =
        ceil_utf8_boundary(input, input.len().saturating_sub(tail_budget)).max(head_end);

    let head = input.get(..head_end).unwrap_or("").trim_end();
    let tail = input.get(tail_start..).unwrap_or("").trim_start();
    let omitted_bytes = tail_start.saturating_sub(head_end);

    let mut out = String::with_capacity(head.len() + tail.len() + marker_overhead);
    out.push_str(head);
    out.push_str(&format!("\n...[omitted {omitted_bytes} bytes]...\n"));
    out.push_str(tail);
    out
}
