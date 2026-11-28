//! Bounded ring buffer of per-call tool I/O served at `/api/telemetry/tool-calls`.
//!
//! Every tool call the gateway sees is recorded here: suspended MCP round-trips, XML calls,
//! hub and dispatcher unwrapping, gateway-local 0-RTT replies, and gateway-hosted tools. The
//! buffer is independent of full-context capture: it never retains conversation history, each
//! stored field is clipped to a per-field cap, and the whole buffer is bounded by record count
//! and total bytes, evicting the oldest records first.

use std::collections::{HashMap, VecDeque};

use liftgate_tool_virtualizer::strip_call_prefix;
use liftgate_types::{
    canonical::{CanonicalMessage, CanonicalPart, CanonicalRole},
    session_key::{scrub_terminal_control_noise, strip_billing_noise, truncate_head_tail_lines},
    telemetry::{
        ToolCallDetail, ToolCallErrorKind, ToolCallRecord, ToolCallRoute, ToolCallStatus,
        ToolCallTelemetrySummary, ToolCallsResponse, ToolOutputBlock,
    },
};

/// Default number of retained tool calls.
pub const DEFAULT_MAX_TOOL_CALL_RECORDS: usize = 2_000;
/// Default cap applied separately to stored arguments and output text of one call.
pub const DEFAULT_MAX_TOOL_CALL_FIELD_BYTES: usize = 128 * 1024;
/// Default cap on the bytes retained across the whole buffer.
pub const DEFAULT_MAX_TOOL_CALL_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// A call still pending after this long is reported as abandoned.
pub const PENDING_TOOL_CALL_TTL_MS: u64 = 30 * 60 * 1000;

const PREVIEW_CHARS: usize = 240;
/// Cap on each id, name and MIME type kept per call. These come from peers (model-emitted tool
/// names, client-chosen model ids), so they are bounded like every other retained field.
const MAX_LABEL_BYTES: usize = 256;
/// Fixed per-record bookkeeping charged against the total byte budget.
const RECORD_OVERHEAD_BYTES: usize = 512;
/// Base64 prefix decoded when probing image dimensions (enough for JPEG SOF markers).
const IMAGE_PROBE_BASE64_CHARS: usize = 96 * 1024;
/// Largest base64 tool-result image retained as an inline dashboard preview.
pub const MAX_TOOL_IMAGE_PREVIEW_BASE64_BYTES: usize = 384 * 1024;
/// Raster formats safe to render back as `data:` URL previews (no SVG / scriptable formats).
const PREVIEWABLE_IMAGE_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
];

/// Returns a copy of `data` when it is a raster image small enough to keep as a preview.
///
/// Rejects non-raster media types, empty or oversized payloads, and anything outside the
/// base64 alphabet so the dashboard can safely embed it in a `data:` URL.
#[must_use]
pub fn previewable_image_data(media_type: &str, data: &str, max_bytes: usize) -> Option<String> {
    let media_type = media_type.trim();
    let raster = PREVIEWABLE_IMAGE_TYPES
        .iter()
        .any(|known| media_type.eq_ignore_ascii_case(known));
    if !raster || data.is_empty() || data.len() > max_bytes {
        return None;
    }
    let base64 = data
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\r' | b'\n'));
    base64.then(|| data.to_owned())
}

const _: () = assert!(
    DEFAULT_MAX_TOOL_CALL_FIELD_BYTES * 4 < DEFAULT_MAX_TOOL_CALL_TOTAL_BYTES,
    "a single record must never exceed the total tool-call budget"
);

/// Caller-settable bounds for [`ToolCallStore`]; `max_records == 0` disables recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCallLimits {
    pub max_records: usize,
    pub max_field_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for ToolCallLimits {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_MAX_TOOL_CALL_RECORDS,
            max_field_bytes: DEFAULT_MAX_TOOL_CALL_FIELD_BYTES,
            max_total_bytes: DEFAULT_MAX_TOOL_CALL_TOTAL_BYTES,
        }
    }
}

/// Task-level attribution shared by every call a turn emits.
#[derive(Clone, Debug, Default)]
pub struct ToolCallTaskContext {
    pub task_id: String,
    pub provider_id: String,
    pub session_id: Option<String>,
    pub model_id: String,
    pub harness_profile: &'static str,
    pub orchestrator_mode: Option<&'static str>,
    pub turn_index: u64,
}

/// Everything known about a call when the model emits it.
#[derive(Clone, Debug)]
pub struct ToolCallStart {
    pub call_id: String,
    pub route: ToolCallRoute,
    pub wire_tool_name: String,
    pub client_tool_name: Option<String>,
    pub is_orchestrator: bool,
    pub code_language: Option<String>,
    pub wire_arguments: String,
    /// Arguments forwarded to the client when they differ from `wire_arguments`.
    pub client_arguments: Option<String>,
    pub v4a_auto_repaired: bool,
}

/// Image attached to a tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutputImage {
    pub mime_type: String,
    pub base64_bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bounded base64 preview (see [`previewable_image_data`]).
    pub data: Option<String>,
}

impl ToolOutputImage {
    /// Describes a base64 image block, probing PNG / GIF / JPEG / WebP headers for its size.
    #[must_use]
    pub fn probe(mime_type: &str, base64_data: &str) -> Self {
        let dims = image_dimensions(base64_data);
        Self {
            mime_type: capped_label(mime_type),
            base64_bytes: base64_data.len(),
            width: dims.map(|(w, _)| w),
            height: dims.map(|(_, h)| h),
            data: previewable_image_data(
                mime_type,
                base64_data,
                MAX_TOOL_IMAGE_PREVIEW_BASE64_BYTES,
            ),
        }
    }
}

/// How a call ended.
#[derive(Clone, Debug)]
pub struct ToolCallOutcome {
    pub status: ToolCallStatus,
    pub error_kind: Option<ToolCallErrorKind>,
    /// Readable output (billing noise and terminal control sequences removed).
    pub text: String,
    /// Bytes of the output exactly as produced.
    pub raw_bytes: usize,
    pub sanitized: bool,
    pub spilled: bool,
    /// Exact text handed back to the model when the gateway spilled or annotated the output.
    pub forwarded_output: Option<String>,
    pub images: Vec<ToolOutputImage>,
}

impl ToolCallOutcome {
    /// Outcome of a client round-trip, classifying script failures the client did not flag.
    /// Errors of CodeMode orchestrators are refined to script exceptions when completed.
    #[must_use]
    pub fn from_client(raw: &str, is_error: bool) -> Self {
        let (text, sanitized) = readable_output(raw);
        let script_failed = reports_script_failure(&text);
        let (status, error_kind) = if is_error || script_failed {
            let kind = if script_failed {
                ToolCallErrorKind::ScriptException
            } else {
                ToolCallErrorKind::ToolError
            };
            (ToolCallStatus::Error, Some(kind))
        } else {
            (ToolCallStatus::Ok, None)
        };
        Self {
            status,
            error_kind,
            text,
            raw_bytes: raw.len(),
            sanitized,
            spilled: false,
            forwarded_output: None,
            images: Vec::new(),
        }
    }

    /// Outcome of a reply produced inside the gateway.
    #[must_use]
    pub fn local(
        text: String,
        status: ToolCallStatus,
        error_kind: Option<ToolCallErrorKind>,
    ) -> Self {
        Self {
            status,
            error_kind,
            raw_bytes: text.len(),
            text,
            sanitized: false,
            spilled: false,
            forwarded_output: None,
            images: Vec::new(),
        }
    }
}

/// Removes billing noise and terminal control sequences, reporting whether anything changed.
#[must_use]
pub fn readable_output(raw: &str) -> (String, bool) {
    let billing_clean = strip_billing_noise(raw);
    let term_clean = scrub_terminal_control_noise(&billing_clean);
    let changed = term_clean.len() != raw.len() || term_clean.as_ref() != raw;
    (term_clean.into_owned(), changed)
}

/// `true` when a CodeMode result envelope reports a failed script (`Script failed`, `Script error:`).
#[must_use]
pub fn reports_script_failure(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("Script failed")
        || head
            .get(..head.len().min(400))
            .is_some_and(|prefix| prefix.contains("\nScript error:"))
}

fn looks_like_exception(text: &str) -> bool {
    let head = text.get(..text.len().min(2_000)).unwrap_or(text);
    head.contains("Traceback (most recent call last)")
        || head.lines().take(6).any(|line| {
            let line = line.trim_start();
            line.contains("Error:") || line.contains("Exception:") || line.starts_with("Uncaught ")
        })
}

/// Script language of a CodeMode call when the arguments or tool kind reveal it.
#[must_use]
pub fn script_language(is_custom: bool, arguments: &str) -> Option<String> {
    if is_custom {
        return Some("js".to_owned());
    }
    let parsed = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let obj = parsed.as_object()?;
    let declared = ["language", "lang", "runtime"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(serde_json::Value::as_str));
    match declared.map(str::to_ascii_lowercase).as_deref() {
        Some("py" | "python" | "python3") => Some("py".to_owned()),
        Some("js" | "javascript" | "ts" | "typescript" | "node" | "bun") => Some("js".to_owned()),
        Some("sh" | "bash" | "shell") => Some("sh".to_owned()),
        Some(_) | None => obj.contains_key("code").then(|| "js".to_owned()),
    }
}

/// Patch body of a patch tool call, whether it arrived as raw V4A text or inside a JSON field.
fn extract_patch_field(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.starts_with('{')
        && let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
    {
        for key in ["patch", "patchText", "patch_text", "input"] {
            if let Some(text) = map.get(key).and_then(serde_json::Value::as_str) {
                return text.replace("\r\n", "\n").trim().to_owned();
            }
        }
    }
    trimmed.replace("\r\n", "\n")
}

/// `true` when the gateway rewrote a V4A patch (fences, missing envelope, merged hunks) before
/// forwarding it to the client.
#[must_use]
pub fn patch_was_repaired(client_tool: &str, wire_arguments: &str, client_arguments: &str) -> bool {
    let name = client_tool.trim();
    let is_patch_tool =
        name.eq_ignore_ascii_case("apply_patch") || name.eq_ignore_ascii_case("patch");
    is_patch_tool && extract_patch_field(wire_arguments) != extract_patch_field(client_arguments)
}

/// A tool result observed in an incoming request.
#[derive(Clone, Debug)]
pub struct ObservedToolResult<'a> {
    pub call_id: &'a str,
    pub output: &'a str,
    pub is_error: bool,
    pub images: Vec<ToolOutputImage>,
}

/// Tool results in the trailing user messages of a request (after the last assistant message),
/// with the image blocks that follow each result attributed to it.
#[must_use]
pub fn trailing_tool_results(messages: &[CanonicalMessage]) -> Vec<ObservedToolResult<'_>> {
    let start = messages
        .iter()
        .rposition(|m| m.role == CanonicalRole::Assistant)
        .map_or(0, |idx| idx.saturating_add(1));
    let mut out: Vec<ObservedToolResult<'_>> = Vec::new();
    for msg in messages.get(start..).unwrap_or(&[]) {
        let mut attach_to: Option<usize> = None;
        for part in &msg.parts {
            match part {
                CanonicalPart::ToolResult {
                    call_id,
                    output,
                    is_error,
                } => {
                    attach_to = Some(out.len());
                    out.push(ObservedToolResult {
                        call_id,
                        output,
                        is_error: *is_error,
                        images: Vec::new(),
                    });
                }
                CanonicalPart::ImageBase64 { media_type, data } => {
                    if let Some(result) = attach_to.and_then(|idx| out.get_mut(idx)) {
                        result.images.push(ToolOutputImage::probe(media_type, data));
                    }
                }
                CanonicalPart::Text(_)
                | CanonicalPart::Reasoning(_)
                | CanonicalPart::ToolCall { .. } => {
                    attach_to = None;
                }
            }
        }
    }
    out
}

struct ToolCallEntry {
    key: String,
    record: ToolCallRecord,
    wire_arguments: String,
    client_arguments: Option<String>,
    output_blocks: Vec<ToolOutputBlock>,
    forwarded_output: Option<String>,
    forwarded_output_bytes: Option<usize>,
    input_truncated: bool,
    output_truncated: bool,
    forwarded_output_truncated: bool,
    retained_bytes: usize,
}

/// Copy of `value` cut to [`MAX_LABEL_BYTES`] on a char boundary, marked with `…` when cut.
fn capped_label(value: &str) -> String {
    if value.len() <= MAX_LABEL_BYTES {
        return value.to_owned();
    }
    let end = value.floor_char_boundary(MAX_LABEL_BYTES.saturating_sub('…'.len_utf8()));
    let mut out = String::with_capacity(MAX_LABEL_BYTES);
    out.push_str(value.get(..end).unwrap_or_default());
    out.push('…');
    out
}

/// [`capped_label`] for owned values, reusing the allocation when nothing is cut.
fn cap_label(value: String) -> String {
    if value.len() <= MAX_LABEL_BYTES {
        value
    } else {
        capped_label(&value)
    }
}

/// Lookup key for a call id: prefix-stripped and bounded exactly like stored keys.
fn call_key(call_id: &str) -> &str {
    let key = strip_call_prefix(call_id);
    key.get(..key.floor_char_boundary(MAX_LABEL_BYTES))
        .unwrap_or(key)
}

impl ToolCallEntry {
    fn measure(&mut self) {
        let record = &self.record;
        let labels = [
            self.key.as_str(),
            record.call_id.as_str(),
            record.task_id.as_str(),
            record.session_id.as_deref().unwrap_or_default(),
            record.model_id.as_str(),
            record.harness_profile.as_str(),
            record.orchestrator_mode.as_deref().unwrap_or_default(),
            record.wire_tool_name.as_str(),
            record.client_tool_name.as_deref().unwrap_or_default(),
            record.code_language.as_deref().unwrap_or_default(),
        ]
        .iter()
        .map(|label| label.len())
        .fold(0_usize, usize::saturating_add);
        let blocks: usize = self
            .output_blocks
            .iter()
            .map(|b| match b {
                ToolOutputBlock::Text { text, .. } => text.len(),
                ToolOutputBlock::Image {
                    mime_type, data, ..
                } => mime_type
                    .len()
                    .saturating_add(16)
                    .saturating_add(data.as_deref().map_or(0, str::len)),
            })
            .sum();
        self.retained_bytes = RECORD_OVERHEAD_BYTES
            .saturating_add(self.wire_arguments.len())
            .saturating_add(self.client_arguments.as_deref().map_or(0, str::len))
            .saturating_add(self.forwarded_output.as_deref().map_or(0, str::len))
            .saturating_add(self.record.input_preview.len())
            .saturating_add(self.record.output_preview.len())
            .saturating_add(labels)
            .saturating_add(blocks);
    }
}

/// Bounded FIFO store of tool calls keyed by their prefix-stripped call id.
pub struct ToolCallStore {
    /// Random per buffer, so clients can tell a restarted gateway from a quiet one.
    store_id: String,
    limits: ToolCallLimits,
    entries: VecDeque<ToolCallEntry>,
    by_key: HashMap<String, u64>,
    next_seq: u64,
    revision: u64,
    stored_bytes: usize,
    pending: usize,
    total_recorded: u64,
    total_evicted: u64,
}

impl ToolCallStore {
    #[must_use]
    pub fn new(limits: ToolCallLimits) -> Self {
        Self {
            store_id: uuid::Uuid::new_v4().simple().to_string(),
            limits,
            entries: VecDeque::new(),
            by_key: HashMap::new(),
            next_seq: 1,
            revision: 0,
            stored_bytes: 0,
            pending: 0,
            total_recorded: 0,
            total_evicted: 0,
        }
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.limits.max_records > 0
    }

    #[must_use]
    pub const fn pending_count(&self) -> usize {
        self.pending
    }

    #[must_use]
    pub fn summary(&self) -> ToolCallTelemetrySummary {
        ToolCallTelemetrySummary {
            store_id: self.store_id.clone(),
            revision: self.revision,
            oldest_seq: self.entries.front().map_or(self.next_seq, |e| e.record.seq),
            max_records: self.limits.max_records,
            max_field_bytes: self.limits.max_field_bytes,
            max_total_bytes: self.limits.max_total_bytes,
            stored_records: self.entries.len(),
            stored_bytes: self.stored_bytes,
            total_recorded: self.total_recorded,
            total_evicted: self.total_evicted,
        }
    }

    /// Records changed after `since` (every retained record when `None`), oldest first.
    #[must_use]
    pub fn list(&self, since: Option<u64>) -> ToolCallsResponse {
        let floor = since.unwrap_or(0);
        ToolCallsResponse {
            summary: self.summary(),
            records: self
                .entries
                .iter()
                .filter(|e| e.record.revision > floor)
                .map(|e| e.record.clone())
                .collect(),
        }
    }

    #[must_use]
    pub fn detail(&self, call_id: &str) -> Option<ToolCallDetail> {
        let entry = self.entry(call_key(call_id))?;
        Some(ToolCallDetail {
            record: entry.record.clone(),
            wire_arguments: entry.wire_arguments.clone(),
            client_arguments: entry.client_arguments.clone(),
            output_blocks: entry.output_blocks.clone(),
            forwarded_output: entry.forwarded_output.clone(),
            forwarded_output_bytes: entry.forwarded_output_bytes,
            input_truncated: entry.input_truncated,
            output_truncated: entry.output_truncated,
            forwarded_output_truncated: entry.forwarded_output_truncated,
        })
    }

    fn position_of(&self, seq: u64) -> Option<usize> {
        let front = self.entries.front()?.record.seq;
        let offset = seq.checked_sub(front)?;
        usize::try_from(offset).ok()
    }

    fn entry(&self, key: &str) -> Option<&ToolCallEntry> {
        let seq = *self.by_key.get(key)?;
        self.entries.get(self.position_of(seq)?)
    }

    fn entry_mut(&mut self, key: &str) -> Option<&mut ToolCallEntry> {
        let seq = *self.by_key.get(key)?;
        let pos = self.position_of(seq)?;
        self.entries.get_mut(pos)
    }

    fn bump(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }

    fn clip(&self, raw: &str, label: &str) -> (String, bool) {
        if raw.len() <= self.limits.max_field_bytes {
            (raw.to_owned(), false)
        } else {
            (
                truncate_head_tail_lines(raw, self.limits.max_field_bytes, label),
                true,
            )
        }
    }

    fn clip_forwarded(
        max_field: usize,
        readable_text: &str,
        forwarded: Option<&str>,
        spilled: bool,
    ) -> (Option<String>, Option<usize>, bool) {
        let Some(fwd) = forwarded else {
            return (None, None, false);
        };
        if !spilled && fwd == readable_text {
            return (None, None, false);
        }
        let bytes = fwd.len();
        if bytes <= max_field {
            (Some(fwd.to_owned()), Some(bytes), false)
        } else {
            (
                Some(truncate_head_tail_lines(
                    fwd,
                    max_field,
                    "forwarded tool call output",
                )),
                Some(bytes),
                true,
            )
        }
    }

    /// Records a call the model just emitted; it stays `pending` until [`Self::complete`].
    pub fn begin(
        &mut self,
        ctx: &ToolCallTaskContext,
        step_index: u32,
        start: ToolCallStart,
        now_ms: u64,
    ) {
        if !self.is_enabled() {
            return;
        }
        let ToolCallStart {
            call_id,
            route,
            wire_tool_name,
            client_tool_name,
            is_orchestrator,
            code_language,
            wire_arguments,
            client_arguments,
            v4a_auto_repaired,
        } = start;
        let key = call_key(&call_id).to_owned();
        let input_bytes = client_arguments.as_deref().unwrap_or(&wire_arguments).len();
        let input_preview = preview(client_arguments.as_deref().unwrap_or(&wire_arguments));
        let (wire_stored, wire_cut) = self.clip(&wire_arguments, "tool call arguments");
        let (client_stored, client_cut) = match client_arguments.as_deref() {
            Some(args) => {
                let (stored, cut) = self.clip(args, "forwarded tool call arguments");
                (Some(stored), cut)
            }
            None => (None, false),
        };
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let revision = self.bump();
        let mut entry = ToolCallEntry {
            key: key.clone(),
            record: ToolCallRecord {
                call_id: cap_label(call_id),
                seq,
                revision,
                task_id: capped_label(&ctx.task_id),
                provider_id: if ctx.provider_id.is_empty() {
                    "agy".to_owned()
                } else {
                    capped_label(&ctx.provider_id)
                },
                session_id: ctx.session_id.as_deref().map(capped_label),
                turn_index: ctx.turn_index,
                step_index,
                harness_profile: ctx.harness_profile.to_owned(),
                orchestrator_mode: ctx.orchestrator_mode.map(str::to_owned),
                model_id: capped_label(&ctx.model_id),
                route,
                wire_tool_name: cap_label(wire_tool_name),
                client_tool_name: client_tool_name.map(cap_label),
                is_orchestrator,
                code_language: code_language.map(cap_label),
                status: ToolCallStatus::Pending,
                error_kind: None,
                v4a_auto_repaired,
                output_sanitized: false,
                output_spilled: false,
                started_at_unix_ms: now_ms,
                duration_ms: None,
                input_bytes,
                output_bytes: 0,
                input_tokens_est: estimate_tokens(input_bytes),
                output_tokens_est: 0,
                input_preview,
                output_preview: String::new(),
            },
            wire_arguments: wire_stored,
            client_arguments: client_stored,
            output_blocks: Vec::new(),
            forwarded_output: None,
            forwarded_output_bytes: None,
            input_truncated: wire_cut || client_cut,
            output_truncated: false,
            forwarded_output_truncated: false,
            retained_bytes: 0,
        };
        entry.measure();
        self.stored_bytes = self.stored_bytes.saturating_add(entry.retained_bytes);
        self.pending = self.pending.saturating_add(1);
        self.total_recorded = self.total_recorded.saturating_add(1);
        if let Some(old_seq) = self.by_key.insert(key, seq)
            && let Some(pos) = self.position_of(old_seq)
            && let Some(old) = self.entries.get_mut(pos)
            && old.record.status == ToolCallStatus::Pending
        {
            // A reused call id supersedes the earlier pending record.
            old.record.status = ToolCallStatus::Abandoned;
            self.pending = self.pending.saturating_sub(1);
        }
        self.entries.push_back(entry);
        self.evict_over_budget();
    }

    /// Completes the call identified by `call_id` (any `call_` / `toolu_` prefix). Completing an
    /// already finished call replaces its output, so a richer completion can refine a raw one.
    pub fn complete(&mut self, call_id: &str, outcome: ToolCallOutcome, now_ms: u64) -> bool {
        let key = call_key(call_id).to_owned();
        let max_field = self.limits.max_field_bytes;
        let revision = self.revision.saturating_add(1);
        let Some(entry) = self.entry_mut(&key) else {
            return false;
        };
        let was_pending = entry.record.status == ToolCallStatus::Pending;
        let ToolCallOutcome {
            status,
            mut error_kind,
            text,
            raw_bytes,
            sanitized,
            spilled,
            forwarded_output,
            images,
        } = outcome;
        let (fwd_stored, fwd_bytes, fwd_cut) =
            Self::clip_forwarded(max_field, &text, forwarded_output.as_deref(), spilled);
        let (stored_text, cut) = if text.len() <= max_field {
            (text, false)
        } else {
            (
                truncate_head_tail_lines(&text, max_field, "tool call output"),
                true,
            )
        };
        if error_kind == Some(ToolCallErrorKind::ToolError)
            && entry.record.is_orchestrator
            && looks_like_exception(&stored_text)
        {
            error_kind = Some(ToolCallErrorKind::ScriptException);
        }
        let image_bytes: usize = images.iter().map(|img| img.base64_bytes).sum();
        let output_bytes = raw_bytes.saturating_add(image_bytes);
        entry.record.status = status;
        entry.record.error_kind = error_kind;
        entry.record.output_sanitized = sanitized;
        entry.record.output_spilled |= spilled;
        if was_pending || entry.record.duration_ms.is_none() {
            entry.record.duration_ms = Some(now_ms.saturating_sub(entry.record.started_at_unix_ms));
        }
        entry.record.output_bytes = output_bytes;
        entry.record.output_tokens_est = estimate_tokens(output_bytes);
        entry.record.output_preview = preview(&stored_text);
        entry.record.revision = revision;
        entry.output_truncated = cut;
        if fwd_stored.is_some() {
            entry.forwarded_output = fwd_stored;
            entry.forwarded_output_bytes = fwd_bytes;
            entry.forwarded_output_truncated = fwd_cut;
        }
        let mut blocks = Vec::with_capacity(1 + images.len());
        if !stored_text.is_empty() || images.is_empty() {
            blocks.push(ToolOutputBlock::Text {
                bytes: raw_bytes,
                text: stored_text,
            });
        }
        blocks.extend(images.into_iter().map(|img| ToolOutputBlock::Image {
            mime_type: img.mime_type,
            base64_bytes: img.base64_bytes,
            width: img.width,
            height: img.height,
            data: img.data,
        }));
        entry.output_blocks = blocks;
        let before = entry.retained_bytes;
        entry.measure();
        let after = entry.retained_bytes;
        self.revision = revision;
        self.stored_bytes = self
            .stored_bytes
            .saturating_sub(before)
            .saturating_add(after);
        if was_pending {
            self.pending = self.pending.saturating_sub(1);
        }
        self.evict_over_budget();
        true
    }

    /// `Some(true)` while the call awaits its result, `Some(false)` once finished, `None` if unknown.
    #[must_use]
    pub fn is_pending(&self, call_id: &str) -> Option<bool> {
        self.entry(call_key(call_id))
            .map(|e| e.record.status == ToolCallStatus::Pending)
    }

    /// Flags a finished call whose output the gateway spilled before replying to the model.
    pub fn mark_spilled(&mut self, call_id: &str) -> bool {
        self.record_forwarded_output(call_id, None, true)
    }

    /// Attaches the gateway-shaped reply text (and spill state) to a call already completed from
    /// incoming messages.
    pub fn record_forwarded_output(
        &mut self,
        call_id: &str,
        forwarded: Option<&str>,
        spilled: bool,
    ) -> bool {
        let key = call_key(call_id).to_owned();
        let max_field = self.limits.max_field_bytes;
        let revision = self.revision.saturating_add(1);
        let Some(entry) = self.entry_mut(&key) else {
            return false;
        };
        let primary_text = entry
            .output_blocks
            .iter()
            .find_map(|b| match b {
                ToolOutputBlock::Text { text, .. } => Some(text.as_str()),
                ToolOutputBlock::Image { .. } => None,
            })
            .unwrap_or_default();
        let (fwd_stored, fwd_bytes, fwd_cut) =
            Self::clip_forwarded(max_field, primary_text, forwarded, spilled);
        let spill_added = spilled && !entry.record.output_spilled;
        let fwd_changed = fwd_stored.is_some() && entry.forwarded_output != fwd_stored;
        if !spill_added && !fwd_changed {
            return false;
        }
        entry.record.output_spilled |= spilled;
        if fwd_stored.is_some() {
            entry.forwarded_output = fwd_stored;
            entry.forwarded_output_bytes = fwd_bytes;
            entry.forwarded_output_truncated = fwd_cut;
        }
        entry.record.revision = revision;
        let before = entry.retained_bytes;
        entry.measure();
        let after = entry.retained_bytes;
        self.revision = revision;
        self.stored_bytes = self
            .stored_bytes
            .saturating_sub(before)
            .saturating_add(after);
        self.evict_over_budget();
        true
    }

    /// Records a call that started and finished inside the gateway.
    pub fn record_local(
        &mut self,
        ctx: &ToolCallTaskContext,
        step_index: u32,
        start: ToolCallStart,
        outcome: ToolCallOutcome,
        started_ms: u64,
        now_ms: u64,
    ) {
        let call_id = start.call_id.clone();
        self.begin(ctx, step_index, start, started_ms);
        let _ = self.complete(&call_id, outcome, now_ms);
    }

    /// Marks still-pending calls as abandoned (their result will never arrive).
    pub fn abandon(&mut self, call_ids: &[String]) {
        for call_id in call_ids {
            let key = call_key(call_id).to_owned();
            let revision = self.revision.saturating_add(1);
            let Some(entry) = self.entry_mut(&key) else {
                continue;
            };
            if entry.record.status != ToolCallStatus::Pending {
                continue;
            }
            entry.record.status = ToolCallStatus::Abandoned;
            entry.record.revision = revision;
            self.revision = revision;
            self.pending = self.pending.saturating_sub(1);
        }
    }

    /// Abandons calls that stayed pending longer than [`PENDING_TOOL_CALL_TTL_MS`].
    pub fn sweep_stale(&mut self, now_ms: u64) {
        if self.pending == 0 {
            return;
        }
        let cutoff = now_ms.saturating_sub(PENDING_TOOL_CALL_TTL_MS);
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|e| {
                e.record.status == ToolCallStatus::Pending && e.record.started_at_unix_ms < cutoff
            })
            .map(|e| e.key.clone())
            .collect();
        if !stale.is_empty() {
            self.abandon(&stale);
        }
    }

    /// Completes pending calls whose results arrived in `messages`.
    pub fn complete_from_messages(&mut self, messages: &[CanonicalMessage], now_ms: u64) -> usize {
        if self.pending == 0 {
            return 0;
        }
        let mut completed = 0usize;
        for observed in trailing_tool_results(messages) {
            if self.is_pending(observed.call_id) != Some(true) {
                continue;
            }
            let mut outcome = ToolCallOutcome::from_client(observed.output, observed.is_error);
            outcome.images = observed.images;
            if self.complete(observed.call_id, outcome, now_ms) {
                completed = completed.saturating_add(1);
            }
        }
        completed
    }

    fn evict_over_budget(&mut self) {
        while self.entries.len() > self.limits.max_records
            || (self.stored_bytes > self.limits.max_total_bytes && self.entries.len() > 1)
        {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.stored_bytes = self.stored_bytes.saturating_sub(old.retained_bytes);
            if old.record.status == ToolCallStatus::Pending {
                self.pending = self.pending.saturating_sub(1);
            }
            if self.by_key.get(&old.key) == Some(&old.record.seq) {
                let _ = self.by_key.remove(&old.key);
            }
            self.total_evicted = self.total_evicted.saturating_add(1);
            self.revision = self.revision.saturating_add(1);
        }
    }
}

fn estimate_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.saturating_add(3) / 4).unwrap_or(u64::MAX)
}

/// One-line preview with whitespace runs collapsed.
fn preview(text: &str) -> String {
    let mut out = String::with_capacity(PREVIEW_CHARS.min(text.len()) + 3);
    let mut chars = 0usize;
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if chars >= PREVIEW_CHARS {
            out.push('…');
            break;
        }
        if pending_space {
            out.push(' ');
            chars = chars.saturating_add(1);
            pending_space = false;
        }
        out.push(ch);
        chars = chars.saturating_add(1);
    }
    out
}

// ---------------------------------------------------------------------------
// Image header probing
// ---------------------------------------------------------------------------

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decodes the leading `max_chars` of a base64 string, skipping whitespace.
fn decode_base64_prefix(data: &str, max_chars: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(max_chars / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data.as_bytes().iter().take(max_chars) {
        if byte == b'=' {
            break;
        }
        let Some(value) = base64_value(byte) else {
            continue;
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    out
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u32::from(u16::from_be_bytes([*b.first()?, *b.get(1)?])))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u32::from(u16::from_le_bytes([*b.first()?, *b.get(1)?])))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([
        *b.first()?,
        *b.get(1)?,
        *b.get(2)?,
        *b.get(3)?,
    ]))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(3)?)?;
    Some(u32::from(*b.first()?) | (u32::from(*b.get(1)?) << 8) | (u32::from(*b.get(2)?) << 16))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    loop {
        if *bytes.get(i)? != 0xFF {
            i = i.checked_add(1)?;
            continue;
        }
        let marker = *bytes.get(i.checked_add(1)?)?;
        match marker {
            0xFF => i = i.checked_add(1)?,
            0xD8 | 0x01 | 0xD0..=0xD7 => i = i.checked_add(2)?,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let height = be_u16(bytes, i.checked_add(5)?)?;
                let width = be_u16(bytes, i.checked_add(7)?)?;
                return Some((width, height));
            }
            _ => {
                let len = usize::try_from(be_u16(bytes, i.checked_add(2)?)?).ok()?;
                i = i.checked_add(2)?.checked_add(len)?;
            }
        }
    }
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        b"VP8 " => Some((le_u16(bytes, 26)? & 0x3FFF, le_u16(bytes, 28)? & 0x3FFF)),
        b"VP8L" => {
            let b1 = u32::from(*bytes.get(21)?);
            let b2 = u32::from(*bytes.get(22)?);
            let b3 = u32::from(*bytes.get(23)?);
            let b4 = u32::from(*bytes.get(24)?);
            let width = 1 + (b1 | ((b2 & 0x3F) << 8));
            let height = 1 + ((b2 >> 6) | (b3 << 2) | ((b4 & 0x0F) << 10));
            Some((width, height))
        }
        b"VP8X" => Some((1 + le_u24(bytes, 24)?, 1 + le_u24(bytes, 27)?)),
        _ => None,
    }
}

/// Width and height of a base64 PNG, GIF, JPEG or WebP image, read from its header only.
#[must_use]
pub fn image_dimensions(base64_data: &str) -> Option<(u32, u32)> {
    let bytes = decode_base64_prefix(base64_data, IMAGE_PROBE_BASE64_CHARS);
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be_u32(&bytes, 16)?, be_u32(&bytes, 20)?));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some((le_u16(&bytes, 6)?, le_u16(&bytes, 8)?));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return jpeg_dimensions(&bytes);
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return webp_dimensions(&bytes);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCallTaskContext {
        ToolCallTaskContext {
            task_id: "resp_1".to_owned(),
            provider_id: "agy".to_owned(),
            session_id: Some("sess-1".to_owned()),
            model_id: "gemini-4-carbon-high".to_owned(),
            harness_profile: "codex",
            orchestrator_mode: Some("codex_exec"),
            turn_index: 3,
        }
    }

    fn start(call_id: &str, args: &str) -> ToolCallStart {
        ToolCallStart {
            call_id: call_id.to_owned(),
            route: ToolCallRoute::ClientRoundtrip,
            wire_tool_name: "mcp_lg_exec".to_owned(),
            client_tool_name: Some("exec".to_owned()),
            is_orchestrator: true,
            code_language: Some("js".to_owned()),
            wire_arguments: args.to_owned(),
            client_arguments: None,
            v4a_auto_repaired: false,
        }
    }

    fn encode_base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk.first().copied().unwrap_or(0),
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    let idx = ((n >> (18 - 6 * i)) & 0x3F) as usize;
                    out.push(char::from(ALPHABET[idx]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn pending_call_completes_by_any_prefixed_id_and_tracks_sizes() {
        let mut store = ToolCallStore::new(ToolCallLimits::default());
        store.begin(
            &ctx(),
            0,
            start("call_abc123", "text(await tools.exec_command({cmd:'ls'}))"),
            1_000,
        );
        assert_eq!(store.pending_count(), 1);
        let summary = store.summary();
        assert_eq!(summary.stored_records, 1);
        assert_eq!(summary.oldest_seq, 1);

        let done = store.complete(
            "toolu_abc123",
            ToolCallOutcome::from_client(
                "Script completed\nWall time 0.1 seconds\nOutput:\nsrc\n",
                false,
            ),
            1_250,
        );
        assert!(done);
        assert_eq!(store.pending_count(), 0);
        let detail = store.detail("call_abc123").expect("detail");
        assert_eq!(detail.record.status, ToolCallStatus::Ok);
        assert_eq!(detail.record.duration_ms, Some(250));
        assert_eq!(detail.record.turn_index, 3);
        assert!(detail.record.output_bytes > 0);
        assert!(
            detail
                .record
                .output_preview
                .starts_with("Script completed Wall time")
        );
        assert!(matches!(
            detail.output_blocks.first(),
            Some(ToolOutputBlock::Text { .. })
        ));
    }

    #[test]
    fn script_failures_without_error_flag_are_still_errors() {
        let outcome = ToolCallOutcome::from_client(
            "Script failed\nWall time 0.0 seconds\nOutput:\n\nScript error:\nSyntaxError: Unexpected identifier",
            false,
        );
        assert_eq!(outcome.status, ToolCallStatus::Error);
        assert_eq!(outcome.error_kind, Some(ToolCallErrorKind::ScriptException));

        let plain = ToolCallOutcome::from_client("permission denied", true);
        assert_eq!(plain.error_kind, Some(ToolCallErrorKind::ToolError));

        let mut store = ToolCallStore::new(ToolCallLimits::default());
        store.begin(&ctx(), 0, start("call_eval", "{}"), 1);
        let traceback = ToolCallOutcome::from_client(
            "Traceback (most recent call last):\n  File \"<eval>\", line 1\nKeyError: 'x'",
            true,
        );
        assert_eq!(traceback.error_kind, Some(ToolCallErrorKind::ToolError));
        let _ = store.complete("call_eval", traceback, 2);
        assert_eq!(
            store.detail("call_eval").and_then(|d| d.record.error_kind),
            Some(ToolCallErrorKind::ScriptException),
            "orchestrator errors that look like exceptions are script exceptions"
        );
    }

    #[test]
    fn ring_buffer_evicts_oldest_by_count_and_bytes_and_drops_their_index() {
        let mut store = ToolCallStore::new(ToolCallLimits {
            max_records: 3,
            max_field_bytes: 1_024,
            max_total_bytes: 1 << 20,
        });
        for i in 0..5 {
            store.begin(&ctx(), 0, start(&format!("call_{i}"), "x"), 1);
        }
        let summary = store.summary();
        assert_eq!(summary.stored_records, 3);
        assert_eq!(summary.total_evicted, 2);
        assert_eq!(summary.oldest_seq, 3);
        assert!(store.detail("call_0").is_none());
        assert!(store.detail("call_4").is_some());
        assert_eq!(store.pending_count(), 3);

        let mut by_bytes = ToolCallStore::new(ToolCallLimits {
            max_records: 100,
            max_field_bytes: 4_096,
            max_total_bytes: 3 * (RECORD_OVERHEAD_BYTES + 4_200),
        });
        let big = "y".repeat(4_000);
        for i in 0..6 {
            by_bytes.begin(&ctx(), 0, start(&format!("call_b{i}"), &big), 1);
        }
        assert!(by_bytes.summary().stored_bytes <= 3 * (RECORD_OVERHEAD_BYTES + 4_200));
        assert!(by_bytes.summary().total_evicted >= 3);
    }

    #[test]
    fn image_previews_keep_only_small_safe_raster_payloads() {
        let png = "iVBORw0KGgoAAAANSUhEUg==";
        assert_eq!(
            previewable_image_data("image/png", png, 1024).as_deref(),
            Some(png)
        );
        assert_eq!(
            previewable_image_data(" IMAGE/JPEG ", png, 1024).as_deref(),
            Some(png)
        );
        assert!(previewable_image_data("image/svg+xml", png, 1024).is_none());
        assert!(previewable_image_data("image/png", png, 4).is_none());
        assert!(previewable_image_data("image/png", "", 1024).is_none());
        assert!(previewable_image_data("image/png", "abc\"><script>", 1024).is_none());
        let probed = ToolOutputImage::probe("image/png", png);
        assert_eq!(probed.data.as_deref(), Some(png));
    }

    #[test]
    fn peer_supplied_labels_are_capped_and_charged_to_the_budget() {
        let huge = "n".repeat(1024 * 1024);
        let mut store = ToolCallStore::new(ToolCallLimits {
            max_records: 100,
            max_field_bytes: 1_024,
            max_total_bytes: 64 * 1024,
        });
        let mut ctx = ctx();
        ctx.model_id = format!("model-{huge}");
        let mut call = start(&format!("call_{huge}"), "{}");
        call.wire_tool_name = format!("mcp_lg_{huge}");
        call.client_tool_name = Some(format!("é{huge}"));
        store.begin(&ctx, 0, call, 1_000);
        let image = ToolOutputImage::probe(&format!("image/{huge}"), "");

        let listed = store.list(None);
        let record = listed.records.first().expect("recorded");
        for label in [
            record.call_id.as_str(),
            record.model_id.as_str(),
            record.wire_tool_name.as_str(),
            record.client_tool_name.as_deref().unwrap_or_default(),
            image.mime_type.as_str(),
        ] {
            assert!(
                label.len() <= MAX_LABEL_BYTES,
                "label kept {} bytes",
                label.len()
            );
            assert!(label.ends_with('…'));
        }
        let stored = listed.summary.stored_bytes;
        assert!(
            stored > RECORD_OVERHEAD_BYTES + 4 * MAX_LABEL_BYTES - 64,
            "labels count toward the budget"
        );
        assert!(stored < RECORD_OVERHEAD_BYTES + 8 * MAX_LABEL_BYTES);

        // The full id still finds the call, and every stored label stays bounded.
        assert_eq!(store.is_pending(&format!("call_{huge}")), Some(true));
        assert!(store.complete(
            &format!("call_{huge}"),
            ToolCallOutcome::local("ok".to_owned(), ToolCallStatus::Ok, None),
            1_200,
        ));
        assert_eq!(store.summary().stored_records, 1);
        assert!(!store.summary().store_id.is_empty());
        assert_ne!(
            store.summary().store_id,
            ToolCallStore::new(ToolCallLimits::default())
                .summary()
                .store_id,
            "each buffer gets its own identity"
        );
    }

    #[test]
    fn oversized_fields_are_clipped_and_flagged() {
        let mut store = ToolCallStore::new(ToolCallLimits {
            max_records: 10,
            max_field_bytes: 2_048,
            max_total_bytes: 1 << 20,
        });
        let huge = (0..2_000)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        store.begin(&ctx(), 0, start("call_big", &huge), 1);
        let _ = store.complete("call_big", ToolCallOutcome::from_client(&huge, false), 2);
        let detail = store.detail("call_big").expect("detail");
        assert!(detail.input_truncated && detail.output_truncated);
        assert!(detail.wire_arguments.len() <= 2_048 + 256);
        assert_eq!(detail.record.input_bytes, huge.len());
        assert_eq!(detail.record.output_bytes, huge.len());
    }

    #[test]
    fn incremental_listing_returns_only_changes_after_since() {
        let mut store = ToolCallStore::new(ToolCallLimits::default());
        store.begin(&ctx(), 0, start("call_a", "1"), 1);
        store.begin(&ctx(), 1, start("call_b", "2"), 1);
        let first = store.list(None);
        assert_eq!(first.records.len(), 2);
        let since = first.summary.revision;
        let _ = store.complete("call_a", ToolCallOutcome::from_client("ok", false), 5);
        let delta = store.list(Some(since));
        assert_eq!(delta.records.len(), 1);
        assert_eq!(
            delta.records.first().map(|r| r.call_id.as_str()),
            Some("call_a")
        );
        assert!(store.list(Some(delta.summary.revision)).records.is_empty());
    }

    #[test]
    fn trailing_results_complete_pending_calls_with_their_images() {
        let png = {
            let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            bytes.extend_from_slice(&1440u32.to_be_bytes());
            bytes.extend_from_slice(&900u32.to_be_bytes());
            bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
            encode_base64(&bytes)
        };
        let messages = vec![
            CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: vec![CanonicalPart::ToolCall {
                    call_id: "call_shot".to_owned(),
                    name: "screenshot".to_owned(),
                    arguments: "{}".to_owned(),
                    is_custom: false,
                }],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![
                    CanonicalPart::ToolResult {
                        call_id: "call_shot".to_owned(),
                        output: "\u{1b}[32msaved\u{1b}[0m".to_owned(),
                        is_error: false,
                    },
                    CanonicalPart::ImageBase64 {
                        media_type: "image/png".to_owned(),
                        data: png,
                    },
                    CanonicalPart::Text("thanks".to_owned()),
                ],
            },
        ];
        let mut store = ToolCallStore::new(ToolCallLimits::default());
        let mut s = start("call_shot", "{}");
        s.is_orchestrator = false;
        store.begin(&ctx(), 0, s, 10);
        assert_eq!(store.complete_from_messages(&messages, 20), 1);
        let detail = store.detail("call_shot").expect("detail");
        assert!(
            detail.record.output_sanitized,
            "ANSI colour codes are scrubbed"
        );
        let image = detail
            .output_blocks
            .iter()
            .find_map(|b| match b {
                ToolOutputBlock::Image { width, height, .. } => Some((*width, *height)),
                ToolOutputBlock::Text { .. } => None,
            })
            .expect("image block");
        assert_eq!(image, (Some(1440), Some(900)));
    }

    #[test]
    fn abandon_and_sweep_resolve_calls_that_never_return() {
        let mut store = ToolCallStore::new(ToolCallLimits::default());
        store.begin(&ctx(), 0, start("call_lost", "1"), 0);
        store.begin(
            &ctx(),
            1,
            start("call_parked", "2"),
            PENDING_TOOL_CALL_TTL_MS,
        );
        store.abandon(&["call_parked".to_owned()]);
        assert_eq!(
            store.detail("call_parked").map(|d| d.record.status),
            Some(ToolCallStatus::Abandoned)
        );
        store.sweep_stale(PENDING_TOOL_CALL_TTL_MS + 1);
        assert_eq!(
            store.detail("call_lost").map(|d| d.record.status),
            Some(ToolCallStatus::Abandoned)
        );
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn detects_patch_repairs_and_script_languages() {
        assert!(patch_was_repaired(
            "apply_patch",
            r#"{"patch":"```diff\n*** Update File: a\n@@\n-a\n+b\n```"}"#,
            "*** Begin Patch\n*** Update File: a\n@@\n-a\n+b\n*** End Patch",
        ));
        assert!(!patch_was_repaired(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** End Patch"}"#,
            "*** Begin Patch\n*** End Patch",
        ));
        assert!(!patch_was_repaired("exec", "a", "b"));
        assert_eq!(script_language(true, "await x"), Some("js".to_owned()));
        assert_eq!(
            script_language(false, r#"{"language":"python","code":"print(1)"}"#),
            Some("py".to_owned())
        );
        assert_eq!(
            script_language(false, r#"{"code":"1"}"#),
            Some("js".to_owned())
        );
        assert_eq!(script_language(false, r#"{"cmd":"ls"}"#), None);
    }

    #[test]
    fn probes_gif_jpeg_and_webp_headers() {
        let gif = encode_base64(b"GIF89a\x40\x01\xf0\x00");
        assert_eq!(image_dimensions(&gif), Some((320, 240)));

        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20]);
        assert_eq!(image_dimensions(&encode_base64(&jpeg)), Some((800, 600)));

        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend_from_slice(&[0; 8]);
        webp.extend_from_slice(&[0x7F, 0x02, 0x00, 0xDF, 0x01, 0x00]);
        assert_eq!(image_dimensions(&encode_base64(&webp)), Some((640, 480)));

        assert_eq!(image_dimensions("bm90IGFuIGltYWdl"), None);
    }

    #[test]
    fn preview_collapses_whitespace_and_bounds_length() {
        assert_eq!(preview("  a\n\n  b\tc  "), "a b c");
        let long = "x".repeat(PREVIEW_CHARS + 50);
        assert_eq!(preview(&long).chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn record_forwarded_output_retains_post_spill_text_and_tracks_bytes() {
        let mut store = ToolCallStore::new(ToolCallLimits::default());
        store.begin(&ctx(), 0, start("call_spill_1", r#"{"path":"big.rs"}"#), 10);
        let raw = "line\n".repeat(4_000);
        let messages = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::ToolResult {
                call_id: "toolu_spill_1".to_owned(),
                output: raw.clone(),
                is_error: false,
            }],
        }];
        assert_eq!(store.complete_from_messages(&messages, 25), 1);
        let before_bytes = store.summary().stored_bytes;

        let forwarded = "line\n... [Gateway truncated tool_result:Read] ...\nline\n";
        assert!(store.record_forwarded_output("call_spill_1", Some(forwarded), true));
        let detail = store.detail("call_spill_1").expect("detail");
        assert!(detail.record.output_spilled);
        assert_eq!(detail.forwarded_output.as_deref(), Some(forwarded));
        assert_eq!(detail.forwarded_output_bytes, Some(forwarded.len()));
        assert!(!detail.forwarded_output_truncated);
        assert_eq!(store.summary().stored_bytes, before_bytes + forwarded.len());
        // Idempotent repeat does not bump revision or double-charge bytes.
        let rev = store.summary().revision;
        assert!(!store.record_forwarded_output("call_spill_1", Some(forwarded), true));
        assert_eq!(store.summary().revision, rev);
    }
}
