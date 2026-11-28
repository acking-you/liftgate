//! Zero-dependency JavaScript normalizer for Code Mode (`mcp_lg_exec` / `mcp_lg_execute`).
//!
//! Handles four deterministic 0-RTT transformations without external dependencies or AST bloat:
//! 1. Strips accidental outer Markdown code fences (` ```js ... ``` `).
//! 2. Auto-escapes unescaped literal backticks inside V4A `apply_patch` template literals.
//! 3. Auto-escapes unescaped inner backticks inside broken general JS template literals
//!    (such as embedded Python/Rust/Markdown heredocs containing `` `lg` `` code spans that
//!    would otherwise fail in V8 with `SyntaxError: Unexpected identifier 'lg'`), while
//!    guaranteeing zero modifications on lexically valid JavaScript programs.
//! 4. Lazily injects a pure-ECMAScript `globalThis.btoa` / `globalThis.atob` polyfill (preserving
//!    any first-line `// @exec:` pragma) whenever the script references `btoa` or `atob` in
//!    Codex's bare V8 isolate (`ReferenceError: btoa is not defined`).

#![forbid(unsafe_code)]

use std::borrow::Cow;

/// Maximum nested template / `${...}` interpolation frames tracked before leaving JS untouched.
const MAX_JS_TEMPLATE_NESTING: usize = 512;

/// Maximum candidate closing-backtick evaluations when repairing broken general templates.
const MAX_JS_HEAL_ATTEMPTS: usize = 256;

/// Maximum sequential broken templates repaired in one script.
const MAX_JS_HEAL_DEPTH: usize = 32;

/// Single-line pure-ECMAScript `btoa` / `atob` polyfill for bare V8 isolates (Latin-1/binary-exact
/// for `0x00..=0xFF` code units, with UTF-8 fallback when code points exceed `0xFF`).
const CODE_MODE_BTOA_ATOB_SHIM: &str = r#"globalThis.btoa ??= (s) => { const str = String(s), chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/", bytes = []; let latin1 = true; for (let i = 0; i < str.length; i++) { if (str.charCodeAt(i) > 255) { latin1 = false; break; } } if (latin1) { for (let i = 0; i < str.length; i++) bytes.push(str.charCodeAt(i)); } else { for (const cpStr of str) { const cp = cpStr.codePointAt(0); if (cp < 0x80) bytes.push(cp); else if (cp < 0x800) bytes.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f)); else if (cp < 0x10000) bytes.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f)); else bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f)); } } let out = ""; for (let i = 0; i < bytes.length; i += 3) { const b0 = bytes[i], b1 = i + 1 < bytes.length ? bytes[i + 1] : 0, b2 = i + 2 < bytes.length ? bytes[i + 2] : 0, n = (b0 << 16) | (b1 << 8) | b2; out += chars[(n >> 18) & 63] + chars[(n >> 12) & 63] + (i + 1 < bytes.length ? chars[(n >> 6) & 63] : "=") + (i + 2 < bytes.length ? chars[n & 63] : "="); } return out; }; globalThis.atob ??= (b64) => { const clean = String(b64).replace(/[\t\n\f\r ]+/g, "").replace(/-/g, "+").replace(/_/g, "/"); if (clean.length % 4 === 1) throw new Error("InvalidCharacterError: invalid base64 length"); const pad = clean.endsWith("==") ? 2 : clean.endsWith("=") ? 1 : 0, chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let out = ""; for (let i = 0; i < clean.length; i += 4) { const c0 = chars.indexOf(clean[i]), c1 = chars.indexOf(clean[i + 1]), c2 = clean[i + 2] === "=" || clean[i + 2] === undefined ? 0 : chars.indexOf(clean[i + 2]), c3 = clean[i + 3] === "=" || clean[i + 3] === undefined ? 0 : chars.indexOf(clean[i + 3]); if (c0 < 0 || c1 < 0 || c2 < 0 || c3 < 0) throw new Error("InvalidCharacterError: invalid base64 string"); const n = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3; out += String.fromCharCode((n >> 16) & 255); if (i + 2 < clean.length - pad) out += String.fromCharCode((n >> 8) & 255); if (i + 3 < clean.length - pad) out += String.fromCharCode(n & 255); } return out; };"#;

/// Normalizes JavaScript source passed to a Code Mode `exec` / `execute` tool:
/// 1. Strips accidental outer markdown code fences (` ```js ... ``` `).
/// 2. Auto-escapes unescaped literal backticks (`` ` `` -> `` \` ``) at `${...}` depth 0 inside
///    broken V4A patch template literals (`` `*** Begin Patch\n...\n*** End Patch` ``).
/// 3. Auto-escapes unescaped inner backticks inside broken general JS template literals (such as
///    embedded Python/Rust/Markdown scripts containing `` `ident` `` spans) while leaving all
///    lexically valid JS scripts untouched.
/// 4. Injects a pure-ECMAScript `globalThis.btoa` / `globalThis.atob` shim (preserving any
///    first-line `// @exec:` pragma) when `btoa` or `atob` is referenced as an identifier.
#[must_use]
pub fn normalize_code_mode_exec_js(raw: &str) -> Cow<'_, str> {
    let trimmed = raw.trim();
    let mut current: Cow<'_, str> =
        if trimmed.starts_with("```") && trimmed.ends_with("```") && trimmed.len() >= 6 {
            Cow::Owned(strip_optional_code_fence(trimmed).to_owned())
        } else {
            Cow::Borrowed(raw)
        };

    if current.contains("*** Begin Patch")
        && let Cow::Owned(healed_v4a) = heal_v4a_template_backticks(&current)
    {
        current = Cow::Owned(healed_v4a);
    }

    if current.contains('`')
        && !scan_js_lexical_validity(&current, false)
        && let Some(healed_general) = heal_general_js_template_backticks(&current)
    {
        current = Cow::Owned(healed_general);
    }

    if script_references_btoa_or_atob(&current) {
        current = Cow::Owned(prepend_btoa_atob_shim(&current));
    }

    current
}

fn strip_optional_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let body = if let Some((_, after_lang_line)) = after_open.split_once('\n') {
            after_lang_line
        } else {
            after_open
        };
        if let Some(stripped) = body.trim_end().strip_suffix("```") {
            return stripped.trim();
        }
    }
    trimmed
}

/// Advances past a quoted JS string (`'` or `"`), returning the byte index right after the
/// closing quote along with whether the string closed cleanly without an unescaped line break.
fn skip_js_quoted_string_checked(src: &str, open_quote_pos: usize, quote: char) -> (usize, bool) {
    let mut pos = open_quote_pos.saturating_add(quote.len_utf8());
    let mut escaped = false;
    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        if escaped {
            if ch == '\r'
                && src
                    .get(pos.saturating_add(1)..)
                    .is_some_and(|r| r.starts_with('\n'))
            {
                pos = pos.saturating_add(2);
            } else {
                pos = pos.saturating_add(ch.len_utf8());
            }
            escaped = false;
            continue;
        }
        if matches!(ch, '\n' | '\r') {
            return (pos, false);
        }
        pos = pos.saturating_add(ch.len_utf8());
        if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return (pos, true);
        }
    }
    (pos, false)
}

fn is_js_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$' || (!ch.is_ascii() && ch.is_alphabetic())
}

fn is_js_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || (!ch.is_ascii() && ch.is_alphanumeric())
}

fn scan_js_ident(src: &str, start: usize) -> (&str, usize) {
    let mut end = start;
    while let Some(tail) = src.get(end..)
        && let Some(ch) = tail.chars().next()
        && is_js_ident_continue(ch)
    {
        end = end.saturating_add(ch.len_utf8());
    }
    (src.get(start..end).unwrap_or_default(), end)
}

fn js_keyword_allows_regex(word: &str) -> bool {
    matches!(
        word,
        "return"
            | "throw"
            | "case"
            | "in"
            | "of"
            | "typeof"
            | "void"
            | "delete"
            | "new"
            | "do"
            | "else"
            | "default"
            | "yield"
            | "await"
            | "instanceof"
    )
}

fn js_keyword_can_precede_ident(word: &str) -> bool {
    matches!(
        word,
        "const"
            | "let"
            | "var"
            | "function"
            | "class"
            | "async"
            | "import"
            | "export"
            | "default"
            | "from"
            | "as"
            | "of"
            | "in"
            | "instanceof"
            | "typeof"
            | "void"
            | "delete"
            | "new"
            | "return"
            | "throw"
            | "case"
            | "yield"
            | "await"
            | "else"
            | "do"
            | "get"
            | "set"
            | "static"
    )
}

fn js_keyword_cannot_tag_template(word: &str) -> bool {
    js_keyword_can_precede_ident(word)
        || matches!(
            word,
            "if" | "for" | "while" | "switch" | "try" | "catch" | "finally" | "break" | "continue"
        )
}

/// Attempts to scan a single-line JS regex literal (`/pattern/flags`) starting at `slash_pos`.
fn try_skip_js_regex_literal(src: &str, slash_pos: usize) -> Option<usize> {
    let tail = src.get(slash_pos..)?;
    if !tail.starts_with('/') || tail.starts_with("//") || tail.starts_with("/*") {
        return None;
    }
    let mut pos = slash_pos.saturating_add(1);
    let mut escaped = false;
    let mut in_class = false;
    while let Some(rem) = src.get(pos..)
        && let Some(ch) = rem.chars().next()
    {
        if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            return None;
        }
        pos = pos.saturating_add(ch.len_utf8());
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            continue;
        }
        if ch == '[' {
            in_class = true;
        } else if ch == '/' {
            while let Some(flag_tail) = src.get(pos..)
                && let Some(flag) = flag_tail.chars().next()
                && flag.is_ascii_alphabetic()
            {
                pos = pos.saturating_add(1);
            }
            return Some(pos);
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsTemplateFrame {
    TemplateBody,
    BraceExpr {
        brace_depth: usize,
        regex_allowed: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsScanOutcome {
    Closed(usize),
    Unclosed(usize),
    DepthExceeded,
}

/// Iteratively scans nested JS template literals and `${ ... }` interpolation expressions without
/// call-stack recursion, enforcing [`MAX_JS_TEMPLATE_NESTING`].
fn run_js_template_scanner(
    src: &str,
    start_pos: usize,
    initial_frame: JsTemplateFrame,
) -> JsScanOutcome {
    let mut frames = Vec::with_capacity(4);
    frames.push(initial_frame);
    let mut pos = start_pos;
    let mut escaped = false;

    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        let Some(top) = frames.last_mut() else {
            return JsScanOutcome::Closed(pos);
        };
        match top {
            JsTemplateFrame::TemplateBody => {
                pos = pos.saturating_add(ch.len_utf8());
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                } else if ch == '`' {
                    frames.pop();
                    if frames.is_empty() {
                        return JsScanOutcome::Closed(pos);
                    }
                } else if ch == '$' && src.get(pos..).is_some_and(|r| r.starts_with('{')) {
                    if frames.len() >= MAX_JS_TEMPLATE_NESTING {
                        return JsScanOutcome::DepthExceeded;
                    }
                    pos = pos.saturating_add(1);
                    frames.push(JsTemplateFrame::BraceExpr {
                        brace_depth: 1,
                        regex_allowed: true,
                    });
                }
            }
            JsTemplateFrame::BraceExpr {
                brace_depth,
                regex_allowed,
            } => match ch {
                ' ' | '\t' | '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                    pos = pos.saturating_add(ch.len_utf8());
                }
                '\'' | '"' => {
                    let (end, closed) = skip_js_quoted_string_checked(src, pos, ch);
                    if !closed {
                        return JsScanOutcome::Unclosed(end);
                    }
                    pos = end;
                    *regex_allowed = false;
                }
                '`' => {
                    *regex_allowed = false;
                    if frames.len() >= MAX_JS_TEMPLATE_NESTING {
                        return JsScanOutcome::DepthExceeded;
                    }
                    pos = pos.saturating_add(1);
                    escaped = false;
                    frames.push(JsTemplateFrame::TemplateBody);
                }
                '/' if tail.starts_with("//") => {
                    pos = tail
                        .find('\n')
                        .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
                }
                '/' if tail.starts_with("/*") => {
                    let Some(end) = src
                        .get(pos.saturating_add(2)..)
                        .and_then(|r| r.find("*/").map(|rel| pos.saturating_add(rel + 4)))
                    else {
                        return JsScanOutcome::Unclosed(src.len());
                    };
                    pos = end;
                }
                '/' if *regex_allowed => {
                    if let Some(end) = try_skip_js_regex_literal(src, pos) {
                        pos = end;
                        *regex_allowed = false;
                    } else {
                        pos = pos.saturating_add(1);
                        *regex_allowed = true;
                    }
                }
                '{' => {
                    *brace_depth = brace_depth.saturating_add(1);
                    pos = pos.saturating_add(1);
                    *regex_allowed = true;
                }
                '}' => {
                    *brace_depth = brace_depth.saturating_sub(1);
                    pos = pos.saturating_add(1);
                    if *brace_depth == 0 {
                        frames.pop();
                        escaped = false;
                        if frames.is_empty() {
                            return JsScanOutcome::Closed(pos);
                        }
                    } else {
                        *regex_allowed = false;
                    }
                }
                ')' | ']' => {
                    pos = pos.saturating_add(1);
                    *regex_allowed = false;
                }
                '+' | '-' if tail.starts_with("++") || tail.starts_with("--") => {
                    pos = pos.saturating_add(2);
                }
                _ if is_js_ident_start(ch) => {
                    let (word, end) = scan_js_ident(src, pos);
                    pos = end;
                    *regex_allowed = js_keyword_allows_regex(word);
                }
                _ if ch.is_ascii_digit() => {
                    pos = skip_js_number_literal(src, pos);
                    *regex_allowed = false;
                }
                _ => {
                    pos = pos.saturating_add(ch.len_utf8());
                    *regex_allowed = true;
                }
            },
        }
    }
    JsScanOutcome::Unclosed(pos)
}

fn skip_js_number_literal(src: &str, start: usize) -> usize {
    let mut pos = start.saturating_add(1);
    while let Some(num_tail) = src.get(pos..)
        && let Some(nc) = num_tail.chars().next()
        && (nc.is_ascii_alphanumeric() || nc == '_' || nc == '.')
    {
        if matches!(nc, 'e' | 'E')
            && src
                .get(pos.saturating_add(1)..)
                .is_some_and(|r| r.starts_with(['+', '-']))
        {
            pos = pos.saturating_add(2);
        } else {
            pos = pos.saturating_add(1);
        }
    }
    pos
}

fn skip_js_brace_expr_checked(src: &str, after_open_brace: usize) -> JsScanOutcome {
    run_js_template_scanner(
        src,
        after_open_brace,
        JsTemplateFrame::BraceExpr {
            brace_depth: 1,
            regex_allowed: true,
        },
    )
}

fn skip_js_template_literal_checked(src: &str, open_tick_pos: usize) -> JsScanOutcome {
    run_js_template_scanner(
        src,
        open_tick_pos.saturating_add(1),
        JsTemplateFrame::TemplateBody,
    )
}

// ============================================================================
// Stage 3: General JS Lexical Validator & Broken Template Backtick Healer
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsLexPrev {
    StmtStart,
    Operator,
    Ident { can_tag: bool, allows_regex: bool },
    QuotedString,
    Atom,
    UntaggedTemplate { orphan_stmt: bool },
    Dot,
}

#[derive(Clone, PartialEq, Eq)]
struct JsLexState {
    prev: JsLexPrev,
    saw_newline: bool,
    saw_space: bool,
    delim_stack: Vec<char>,
}

impl Default for JsLexState {
    fn default() -> Self {
        Self {
            prev: JsLexPrev::StmtStart,
            saw_newline: false,
            saw_space: false,
            delim_stack: Vec::new(),
        }
    }
}

/// Checks whether `tail` can legally follow a closed untagged template literal or quoted string
/// when `saw_newline` indicates whether a line terminator intervened.
fn is_legal_after_string_or_template(
    tail: &str,
    saw_newline: bool,
    orphan_stmt: bool,
    strict_heal_mode: bool,
) -> bool {
    if strict_heal_mode
        && orphan_stmt
        && !tail.starts_with(['.', '[', '+', '?'])
        && !tail.starts_with("?.")
    {
        return false;
    }
    if strict_heal_mode {
        if (tail.starts_with('+') && !tail.starts_with("++") && !tail.starts_with("+-"))
            || tail.starts_with([',', ';', ')', ']', '}', '.', '?', ':'])
            || tail.starts_with("||")
            || tail.starts_with("&&")
            || tail.starts_with("??")
            || tail.starts_with("==")
            || tail.starts_with("!=")
        {
            return true;
        }
        if let Some(ch) = tail.chars().next()
            && is_js_ident_start(ch)
        {
            let (word, _) = scan_js_ident(tail, 0);
            if matches!(word, "in" | "instanceof" | "as" | "satisfies") {
                return true;
            }
        }
        return saw_newline;
    }
    if (tail.starts_with('+') && !tail.starts_with("++"))
        || (tail.starts_with('-') && !tail.starts_with("--"))
        || tail.starts_with([
            ',', ';', ')', ']', '}', '.', '?', ':', '*', '/', '%', '=', '<', '>', '&', '|', '^',
        ])
    {
        return true;
    }
    if let Some(ch) = tail.chars().next()
        && is_js_ident_start(ch)
    {
        let (word, _) = scan_js_ident(tail, 0);
        if matches!(word, "in" | "instanceof" | "as" | "satisfies") {
            return true;
        }
    }
    saw_newline
}

/// Scans `src` from `start_pos` with `state`, returning `Ok(final_state)` if every template,
/// string, comment, delimiter, and token adjacency is lexically valid JS, or `Err(bad_tick_pos)`
/// identifying the first untagged template literal opening position if scanning fails.
fn scan_js_lexical_from(
    src: &str,
    start_pos: usize,
    mut state: JsLexState,
    reject_orphan_stmt_template: bool,
) -> Result<JsLexState, Option<usize>> {
    let mut pos = start_pos;
    let mut first_untagged_tick: Option<usize> = None;

    while let Some(tail) = src.get(pos..).filter(|s| !s.is_empty())
        && let Some(ch) = tail.chars().next()
    {
        if matches!(ch, ' ' | '\t') {
            state.saw_space = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            state.saw_newline = true;
            state.saw_space = true;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if tail.starts_with("//") {
            pos = tail
                .find('\n')
                .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
            state.saw_newline = true;
            state.saw_space = true;
            continue;
        }
        if tail.starts_with("/*") {
            let Some(rel_end) = src.get(pos.saturating_add(2)..).and_then(|r| r.find("*/")) else {
                return Err(first_untagged_tick);
            };
            let comment_body = src
                .get(pos.saturating_add(2)..pos.saturating_add(2 + rel_end))
                .unwrap_or_default();
            if comment_body.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                state.saw_newline = true;
            }
            state.saw_space = true;
            pos = pos.saturating_add(rel_end + 4);
            continue;
        }

        match state.prev {
            JsLexPrev::UntaggedTemplate { orphan_stmt }
                if !is_legal_after_string_or_template(
                    tail,
                    state.saw_newline,
                    orphan_stmt,
                    reject_orphan_stmt_template,
                ) =>
            {
                return Err(first_untagged_tick);
            }
            JsLexPrev::QuotedString
                if !is_legal_after_string_or_template(tail, state.saw_newline, false, false) =>
            {
                return Err(first_untagged_tick);
            }
            _ => {}
        }

        match ch {
            '`' => {
                if matches!(
                    state.prev,
                    JsLexPrev::QuotedString | JsLexPrev::UntaggedTemplate { .. } | JsLexPrev::Dot
                ) && !state.saw_newline
                {
                    return Err(first_untagged_tick);
                }
                let is_tagged = !state.saw_newline
                    && (!reject_orphan_stmt_template || !state.saw_space)
                    && matches!(
                        state.prev,
                        JsLexPrev::Ident { can_tag: true, .. } | JsLexPrev::Atom
                    );
                if !is_tagged && first_untagged_tick.is_none() {
                    first_untagged_tick = Some(pos);
                }
                let orphan_stmt = !is_tagged
                    && (state.prev == JsLexPrev::StmtStart
                        || (state.saw_newline
                            && matches!(
                                state.prev,
                                JsLexPrev::Atom
                                    | JsLexPrev::QuotedString
                                    | JsLexPrev::Ident { can_tag: true, .. }
                                    | JsLexPrev::UntaggedTemplate { .. }
                            )));
                if !validate_template_literal_and_interpolations(
                    src,
                    pos,
                    reject_orphan_stmt_template,
                ) {
                    return Err(first_untagged_tick.or(Some(pos)));
                }
                match skip_js_template_literal_checked(src, pos) {
                    JsScanOutcome::Closed(end) => pos = end,
                    JsScanOutcome::Unclosed(_) | JsScanOutcome::DepthExceeded => {
                        return Err(first_untagged_tick.or(Some(pos)));
                    }
                }
                state.prev = if is_tagged {
                    JsLexPrev::Atom
                } else {
                    JsLexPrev::UntaggedTemplate { orphan_stmt }
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            '\'' | '"' => {
                let (end, closed) = skip_js_quoted_string_checked(src, pos, ch);
                if !closed {
                    return Err(first_untagged_tick);
                }
                pos = end;
                state.prev = JsLexPrev::QuotedString;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '/' => {
                let regex_allowed = matches!(
                    state.prev,
                    JsLexPrev::StmtStart
                        | JsLexPrev::Operator
                        | JsLexPrev::Ident {
                            allows_regex: true,
                            ..
                        }
                ) || state.saw_newline;
                if regex_allowed && let Some(end) = try_skip_js_regex_literal(src, pos) {
                    pos = end;
                    state.prev = JsLexPrev::Atom;
                    state.saw_newline = false;
                    state.saw_space = false;
                } else {
                    pos = pos.saturating_add(if tail.starts_with("/=") { 2 } else { 1 });
                    state.prev = JsLexPrev::Operator;
                    state.saw_newline = false;
                    state.saw_space = false;
                }
            }
            '(' | '[' | '{' => {
                state.delim_stack.push(ch);
                pos = pos.saturating_add(1);
                state.prev = if ch == '{' {
                    JsLexPrev::StmtStart
                } else {
                    JsLexPrev::Operator
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            ')' | ']' | '}' => {
                let expected = match ch {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if state.delim_stack.pop() != Some(expected) {
                    return Err(first_untagged_tick);
                }
                pos = pos.saturating_add(1);
                state.prev = if ch == '}' {
                    JsLexPrev::StmtStart
                } else {
                    JsLexPrev::Atom
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            ';' => {
                pos = pos.saturating_add(1);
                state.prev = JsLexPrev::StmtStart;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '?' if tail.starts_with("?.")
                && !tail
                    .get(2..)
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())) =>
            {
                pos = pos.saturating_add(2);
                state.prev = JsLexPrev::Dot;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '.' => {
                if tail.starts_with("...") {
                    pos = pos.saturating_add(3);
                    state.prev = JsLexPrev::Operator;
                } else if tail
                    .get(1..)
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
                {
                    pos = skip_js_number_literal(src, pos);
                    state.prev = JsLexPrev::Atom;
                } else {
                    pos = pos.saturating_add(1);
                    state.prev = JsLexPrev::Dot;
                }
                state.saw_newline = false;
                state.saw_space = false;
            }
            '#' => {
                // Private identifier `#field` is valid only when immediately followed by an identifier start.
                let Some(after_hash) = src.get(pos.saturating_add(1)..) else {
                    return Err(first_untagged_tick);
                };
                let Some(next_ch) = after_hash.chars().next() else {
                    return Err(first_untagged_tick);
                };
                if !is_js_ident_start(next_ch) {
                    return Err(first_untagged_tick);
                }
                let (_, end) = scan_js_ident(src, pos.saturating_add(1));
                pos = end;
                state.prev = JsLexPrev::Ident {
                    can_tag: true,
                    allows_regex: false,
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            '+' | '-' if tail.starts_with("++") || tail.starts_with("--") => {
                pos = pos.saturating_add(2);
                if matches!(state.prev, JsLexPrev::StmtStart | JsLexPrev::Operator) {
                    state.prev = JsLexPrev::Operator;
                } else {
                    state.prev = JsLexPrev::Atom;
                }
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ if is_js_ident_start(ch) => {
                let (word, end) = scan_js_ident(src, pos);
                pos = end;
                state.prev = JsLexPrev::Ident {
                    can_tag: !js_keyword_cannot_tag_template(word),
                    allows_regex: js_keyword_allows_regex(word),
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ if ch.is_ascii_digit() => {
                pos = skip_js_number_literal(src, pos);
                state.prev = JsLexPrev::Atom;
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ => {
                let Some(op_len) = scan_js_operator_len(tail) else {
                    return Err(first_untagged_tick);
                };
                pos = pos.saturating_add(op_len);
                state.prev = JsLexPrev::Operator;
                state.saw_newline = false;
                state.saw_space = false;
            }
        }
    }

    if reject_orphan_stmt_template
        && matches!(
            state.prev,
            JsLexPrev::UntaggedTemplate { orphan_stmt: true }
        )
    {
        return Err(first_untagged_tick);
    }
    if !state.delim_stack.is_empty() {
        return Err(first_untagged_tick);
    }
    Ok(state)
}

fn scan_js_operator_len(tail: &str) -> Option<usize> {
    if tail.starts_with(">>>=") {
        return Some(4);
    }
    if tail.starts_with("===")
        || tail.starts_with("!==")
        || tail.starts_with(">>>")
        || tail.starts_with("<<=")
        || tail.starts_with(">>=")
        || tail.starts_with("**=")
        || tail.starts_with("&&=")
        || tail.starts_with("||=")
        || tail.starts_with("??=")
    {
        return Some(3);
    }
    if tail.starts_with("==")
        || tail.starts_with("!=")
        || tail.starts_with("<=")
        || tail.starts_with(">=")
        || tail.starts_with("=>")
        || tail.starts_with("&&")
        || tail.starts_with("||")
        || tail.starts_with("??")
        || tail.starts_with("**")
        || tail.starts_with("<<")
        || tail.starts_with(">>")
        || tail.starts_with("+=")
        || tail.starts_with("-=")
        || tail.starts_with("*=")
        || tail.starts_with("%=")
        || tail.starts_with("&=")
        || tail.starts_with("|=")
        || tail.starts_with("^=")
    {
        return Some(2);
    }
    let ch = tail.chars().next()?;
    if matches!(
        ch,
        '+' | '-' | '*' | '%' | '=' | '!' | '<' | '>' | '&' | '|' | '^' | '~' | '?' | ':' | ','
    ) {
        Some(1)
    } else {
        None
    }
}

/// Validates that the template literal opening at `open_tick_pos` closes cleanly and that every
/// `${ ... }` interpolation inside it is also lexically valid JS.
fn validate_template_literal_and_interpolations(
    src: &str,
    open_tick_pos: usize,
    reject_orphan_stmt_template: bool,
) -> bool {
    let mut pos = open_tick_pos.saturating_add(1);
    let mut escaped = false;
    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        if escaped {
            escaped = false;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if ch == '\\' {
            escaped = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if ch == '`' {
            return true;
        }
        if ch == '$'
            && src
                .get(pos.saturating_add(1)..)
                .is_some_and(|r| r.starts_with('{'))
        {
            let expr_start = pos.saturating_add(2);
            let JsScanOutcome::Closed(after_brace) = skip_js_brace_expr_checked(src, expr_start)
            else {
                return false;
            };
            let expr_end = after_brace.saturating_sub(1);
            let Some(expr_slice) = src.get(expr_start..expr_end) else {
                return false;
            };
            if scan_js_lexical_from(
                expr_slice,
                0,
                JsLexState::default(),
                reject_orphan_stmt_template,
            )
            .is_err()
            {
                return false;
            }
            pos = after_brace;
            continue;
        }
        pos = pos.saturating_add(ch.len_utf8());
    }
    false
}

fn scan_js_lexical_validity(src: &str, reject_orphan_stmt_template: bool) -> bool {
    scan_js_lexical_from(src, 0, JsLexState::default(), reject_orphan_stmt_template).is_ok()
}

/// Repairs broken untagged JS template literals containing unescaped inner backticks while
/// leaving `${...}` interpolations and valid sibling templates untouched.
fn heal_general_js_template_backticks(src: &str) -> Option<String> {
    let mut attempts = MAX_JS_HEAL_ATTEMPTS;
    let healed = heal_js_from(src, 0, JsLexState::default(), &mut attempts, 0)?;
    if healed != src && scan_js_lexical_validity(&healed, false) {
        Some(healed)
    } else {
        None
    }
}

fn heal_js_from(
    src: &str,
    start_pos: usize,
    mut state: JsLexState,
    attempts_left: &mut usize,
    depth: usize,
) -> Option<String> {
    if depth > MAX_JS_HEAL_DEPTH {
        return None;
    }

    let mut pos = start_pos;
    while let Some(tail) = src.get(pos..).filter(|s| !s.is_empty())
        && let Some(ch) = tail.chars().next()
    {
        if matches!(ch, ' ' | '\t') {
            state.saw_space = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            state.saw_newline = true;
            state.saw_space = true;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if tail.starts_with("//") {
            pos = tail
                .find('\n')
                .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
            state.saw_newline = true;
            state.saw_space = true;
            continue;
        }
        if tail.starts_with("/*") {
            let rel_end = src
                .get(pos.saturating_add(2)..)
                .and_then(|r| r.find("*/"))?;
            let comment_body = src
                .get(pos.saturating_add(2)..pos.saturating_add(2 + rel_end))
                .unwrap_or_default();
            if comment_body.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                state.saw_newline = true;
            }
            state.saw_space = true;
            pos = pos.saturating_add(rel_end + 4);
            continue;
        }

        match state.prev {
            JsLexPrev::UntaggedTemplate { orphan_stmt }
                if !is_legal_after_string_or_template(
                    tail,
                    state.saw_newline,
                    orphan_stmt,
                    true,
                ) =>
            {
                return None;
            }
            JsLexPrev::QuotedString
                if !is_legal_after_string_or_template(tail, state.saw_newline, false, false) =>
            {
                return None;
            }
            _ => {}
        }

        match ch {
            '`' => {
                if matches!(
                    state.prev,
                    JsLexPrev::QuotedString | JsLexPrev::UntaggedTemplate { .. } | JsLexPrev::Dot
                ) && !state.saw_newline
                {
                    return None;
                }
                let is_tagged = !state.saw_newline
                    && !state.saw_space
                    && matches!(
                        state.prev,
                        JsLexPrev::Ident { can_tag: true, .. } | JsLexPrev::Atom
                    );
                let orphan_stmt = !is_tagged
                    && (state.prev == JsLexPrev::StmtStart
                        || (state.saw_newline
                            && matches!(
                                state.prev,
                                JsLexPrev::Atom
                                    | JsLexPrev::QuotedString
                                    | JsLexPrev::Ident { can_tag: true, .. }
                                    | JsLexPrev::UntaggedTemplate { .. }
                            )));

                let candidates = collect_unescaped_template_ticks(src, pos.saturating_add(1))?;
                for cand_close in candidates {
                    if !candidate_close_has_plausible_follower(src, cand_close, orphan_stmt) {
                        continue;
                    }
                    if *attempts_left == 0 {
                        return None;
                    }
                    *attempts_left = attempts_left.saturating_sub(1);

                    let mut next_state = state.clone();
                    next_state.prev = if is_tagged {
                        JsLexPrev::Atom
                    } else {
                        JsLexPrev::UntaggedTemplate { orphan_stmt }
                    };
                    next_state.saw_newline = false;
                    next_state.saw_space = false;

                    let after_close = cand_close.saturating_add(1);
                    // Fast-path: if the remainder is already lexically valid, no deeper recursion needed!
                    let healed_rem =
                        if scan_js_lexical_from(src, after_close, next_state.clone(), true).is_ok()
                        {
                            src.get(after_close..)?.to_owned()
                        } else if let Some(rem) = heal_js_from(
                            src,
                            after_close,
                            next_state,
                            attempts_left,
                            depth.saturating_add(1),
                        ) {
                            rem
                        } else {
                            continue;
                        };

                    let raw_body = src.get(pos.saturating_add(1)..cand_close)?;
                    let healed_body = match escape_unescaped_template_backticks(raw_body) {
                        TemplateHealOutcome::Unchanged => raw_body.to_owned(),
                        TemplateHealOutcome::Healed(h) => h,
                        TemplateHealOutcome::DepthExceeded => continue,
                    };

                    let prefix = src.get(start_pos..pos.saturating_add(1))?;
                    let mut out = String::with_capacity(
                        prefix.len() + healed_body.len() + 1 + healed_rem.len(),
                    );
                    out.push_str(prefix);
                    out.push_str(&healed_body);
                    out.push('`');
                    out.push_str(&healed_rem);
                    return Some(out);
                }
                return None;
            }
            '\'' | '"' => {
                let (end, closed) = skip_js_quoted_string_checked(src, pos, ch);
                if !closed {
                    return None;
                }
                pos = end;
                state.prev = JsLexPrev::QuotedString;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '/' => {
                let regex_allowed = matches!(
                    state.prev,
                    JsLexPrev::StmtStart
                        | JsLexPrev::Operator
                        | JsLexPrev::Ident {
                            allows_regex: true,
                            ..
                        }
                ) || state.saw_newline;
                if regex_allowed && let Some(end) = try_skip_js_regex_literal(src, pos) {
                    pos = end;
                    state.prev = JsLexPrev::Atom;
                    state.saw_newline = false;
                    state.saw_space = false;
                } else {
                    pos = pos.saturating_add(if tail.starts_with("/=") { 2 } else { 1 });
                    state.prev = JsLexPrev::Operator;
                    state.saw_newline = false;
                    state.saw_space = false;
                }
            }
            '(' | '[' | '{' => {
                state.delim_stack.push(ch);
                pos = pos.saturating_add(1);
                state.prev = if ch == '{' {
                    JsLexPrev::StmtStart
                } else {
                    JsLexPrev::Operator
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            ')' | ']' | '}' => {
                let expected = match ch {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if state.delim_stack.pop() != Some(expected) {
                    return None;
                }
                pos = pos.saturating_add(1);
                state.prev = if ch == '}' {
                    JsLexPrev::StmtStart
                } else {
                    JsLexPrev::Atom
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            ';' => {
                pos = pos.saturating_add(1);
                state.prev = JsLexPrev::StmtStart;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '?' if tail.starts_with("?.")
                && !tail
                    .get(2..)
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())) =>
            {
                pos = pos.saturating_add(2);
                state.prev = JsLexPrev::Dot;
                state.saw_newline = false;
                state.saw_space = false;
            }
            '.' => {
                if tail.starts_with("...") {
                    pos = pos.saturating_add(3);
                    state.prev = JsLexPrev::Operator;
                } else if tail
                    .get(1..)
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
                {
                    pos = skip_js_number_literal(src, pos);
                    state.prev = JsLexPrev::Atom;
                } else {
                    pos = pos.saturating_add(1);
                    state.prev = JsLexPrev::Dot;
                }
                state.saw_newline = false;
                state.saw_space = false;
            }
            '#' => {
                let after_hash = src.get(pos.saturating_add(1)..)?;
                let next_ch = after_hash.chars().next()?;
                if !is_js_ident_start(next_ch) {
                    return None;
                }
                let (_, end) = scan_js_ident(src, pos.saturating_add(1));
                pos = end;
                state.prev = JsLexPrev::Ident {
                    can_tag: true,
                    allows_regex: false,
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            '+' | '-' if tail.starts_with("++") || tail.starts_with("--") => {
                pos = pos.saturating_add(2);
                if matches!(state.prev, JsLexPrev::StmtStart | JsLexPrev::Operator) {
                    state.prev = JsLexPrev::Operator;
                } else {
                    state.prev = JsLexPrev::Atom;
                }
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ if is_js_ident_start(ch) => {
                let (word, end) = scan_js_ident(src, pos);
                pos = end;
                state.prev = JsLexPrev::Ident {
                    can_tag: !js_keyword_cannot_tag_template(word),
                    allows_regex: js_keyword_allows_regex(word),
                };
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ if ch.is_ascii_digit() => {
                pos = skip_js_number_literal(src, pos);
                state.prev = JsLexPrev::Atom;
                state.saw_newline = false;
                state.saw_space = false;
            }
            _ => {
                let op_len = scan_js_operator_len(tail)?;
                pos = pos.saturating_add(op_len);
                state.prev = JsLexPrev::Operator;
                state.saw_newline = false;
                state.saw_space = false;
            }
        }
    }

    if matches!(
        state.prev,
        JsLexPrev::UntaggedTemplate { orphan_stmt: true }
    ) {
        return None;
    }
    if !state.delim_stack.is_empty() {
        return None;
    }
    Some(src.get(start_pos..)?.to_owned())
}

/// Collects all unescaped `` ` `` byte offsets after `body_start` at `${...}` depth 0.
fn collect_unescaped_template_ticks(src: &str, body_start: usize) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    let mut pos = body_start;
    let mut escaped = false;
    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        if escaped {
            escaped = false;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if ch == '\\' {
            escaped = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if ch == '$'
            && src
                .get(pos.saturating_add(1)..)
                .is_some_and(|r| r.starts_with('{'))
        {
            match skip_js_brace_expr_checked(src, pos.saturating_add(2)) {
                JsScanOutcome::Closed(after_brace) => {
                    pos = after_brace;
                    continue;
                }
                JsScanOutcome::Unclosed(_) => {
                    pos = pos.saturating_add(2);
                    continue;
                }
                JsScanOutcome::DepthExceeded => return None,
            }
        }
        if ch == '`' {
            out.push(pos);
        }
        pos = pos.saturating_add(ch.len_utf8());
    }
    Some(out)
}

/// O(1) lookahead rejecting candidate closing backticks that are immediately followed on the
/// same line by an identifier (like `lg` in `` `lg auto-update` ``), number, quote, or backtick.
fn candidate_close_has_plausible_follower(src: &str, cand_close: usize, orphan_stmt: bool) -> bool {
    let mut pos = cand_close.saturating_add(1);
    let mut saw_newline = false;
    while let Some(tail) = src.get(pos..).filter(|s| !s.is_empty())
        && let Some(ch) = tail.chars().next()
    {
        if matches!(ch, ' ' | '\t') {
            pos = pos.saturating_add(1);
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            saw_newline = true;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if tail.starts_with("//") {
            pos = tail
                .find('\n')
                .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
            saw_newline = true;
            continue;
        }
        if tail.starts_with("/*") {
            let Some(rel_end) = src.get(pos.saturating_add(2)..).and_then(|r| r.find("*/")) else {
                return false;
            };
            let comment_body = src
                .get(pos.saturating_add(2)..pos.saturating_add(2 + rel_end))
                .unwrap_or_default();
            if comment_body.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                saw_newline = true;
            }
            pos = pos.saturating_add(rel_end + 4);
            continue;
        }
        return is_legal_after_string_or_template(tail, saw_newline, orphan_stmt, true);
    }
    !orphan_stmt
}

// ============================================================================
// Stage 4: Bare-V8 `btoa` / `atob` Identifier Detection & Shim Injection
// ============================================================================

fn script_references_btoa_or_atob(src: &str) -> bool {
    if (!src.contains("btoa") && !src.contains("atob")) || src.contains("globalThis.btoa ??=") {
        return false;
    }
    code_slice_mentions_btoa_or_atob(src)
}

fn code_slice_mentions_btoa_or_atob(src: &str) -> bool {
    let mut pos = 0_usize;
    let mut regex_allowed = true;
    while let Some(tail) = src.get(pos..).filter(|s| !s.is_empty())
        && let Some(ch) = tail.chars().next()
    {
        match ch {
            ' ' | '\t' | '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                pos = pos.saturating_add(ch.len_utf8());
            }
            '\'' | '"' => {
                pos = skip_js_quoted_string_checked(src, pos, ch).0;
                regex_allowed = false;
            }
            '/' if tail.starts_with("//") => {
                pos = tail
                    .find('\n')
                    .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
            }
            '/' if tail.starts_with("/*") => {
                pos = src
                    .get(pos.saturating_add(2)..)
                    .and_then(|r| r.find("*/").map(|end| pos.saturating_add(end + 4)))
                    .unwrap_or(src.len());
            }
            '/' if regex_allowed => {
                if let Some(end) = try_skip_js_regex_literal(src, pos) {
                    pos = end;
                    regex_allowed = false;
                } else {
                    pos = pos.saturating_add(1);
                    regex_allowed = true;
                }
            }
            '`' => {
                if template_interpolations_mention_btoa_or_atob(src, pos) {
                    return true;
                }
                match skip_js_template_literal_checked(src, pos) {
                    JsScanOutcome::Closed(end) | JsScanOutcome::Unclosed(end) => pos = end,
                    JsScanOutcome::DepthExceeded => return false,
                }
                regex_allowed = false;
            }
            ')' | ']' => {
                pos = pos.saturating_add(1);
                regex_allowed = false;
            }
            _ if is_js_ident_start(ch) => {
                let (word, end) = scan_js_ident(src, pos);
                if matches!(word, "btoa" | "atob") {
                    return true;
                }
                pos = end;
                regex_allowed = js_keyword_allows_regex(word);
            }
            _ if ch.is_ascii_digit() => {
                pos = skip_js_number_literal(src, pos);
                regex_allowed = false;
            }
            _ => {
                pos = pos.saturating_add(ch.len_utf8());
                regex_allowed = true;
            }
        }
    }
    false
}

fn template_interpolations_mention_btoa_or_atob(src: &str, open_tick_pos: usize) -> bool {
    let mut pos = open_tick_pos.saturating_add(1);
    let mut escaped = false;
    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        if escaped {
            escaped = false;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if ch == '\\' {
            escaped = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if ch == '`' {
            return false;
        }
        if ch == '$'
            && src
                .get(pos.saturating_add(1)..)
                .is_some_and(|r| r.starts_with('{'))
        {
            let expr_start = pos.saturating_add(2);
            match skip_js_brace_expr_checked(src, expr_start) {
                JsScanOutcome::Closed(after_brace) => {
                    let expr_end = after_brace.saturating_sub(1);
                    if let Some(expr_slice) = src.get(expr_start..expr_end)
                        && code_slice_mentions_btoa_or_atob(expr_slice)
                    {
                        return true;
                    }
                    pos = after_brace;
                    continue;
                }
                JsScanOutcome::Unclosed(_) | JsScanOutcome::DepthExceeded => return false,
            }
        }
        pos = pos.saturating_add(ch.len_utf8());
    }
    false
}

/// Prepends [`CODE_MODE_BTOA_ATOB_SHIM`] while keeping any first-line `// @exec:` pragma on line 1.
fn prepend_btoa_atob_shim(src: &str) -> String {
    let leading_trimmed = src.trim_start_matches([' ', '\t']);
    if leading_trimmed.starts_with("// @exec:") {
        if let Some(nl_idx) = src.find('\n') {
            let first_line = src.get(..=nl_idx).unwrap_or_default();
            let rest = src.get(nl_idx.saturating_add(1)..).unwrap_or_default();
            let mut out = String::with_capacity(src.len() + CODE_MODE_BTOA_ATOB_SHIM.len() + 2);
            out.push_str(first_line);
            out.push_str(CODE_MODE_BTOA_ATOB_SHIM);
            out.push('\n');
            out.push_str(rest);
            return out;
        }
        let mut out = String::with_capacity(src.len() + CODE_MODE_BTOA_ATOB_SHIM.len() + 2);
        out.push_str(src);
        out.push('\n');
        out.push_str(CODE_MODE_BTOA_ATOB_SHIM);
        return out;
    }

    let mut out = String::with_capacity(src.len() + CODE_MODE_BTOA_ATOB_SHIM.len() + 1);
    out.push_str(CODE_MODE_BTOA_ATOB_SHIM);
    out.push('\n');
    out.push_str(src);
    out
}

// ============================================================================
// Stage 2: Specialized V4A Patch Template Backtick Healer
// ============================================================================

fn skip_v4a_begin_patch_header(src: &str, open_tick_pos: usize) -> Option<usize> {
    let body_start = open_tick_pos.saturating_add(1);
    let after_open = src.get(body_start..)?;
    let mut trim_ws = after_open;
    loop {
        if let Some(rest) = trim_ws.strip_prefix("\\n") {
            trim_ws = rest;
        } else if let Some(rest) = trim_ws.strip_prefix("\\r") {
            trim_ws = rest;
        } else {
            let next = trim_ws.trim_start_matches([' ', '\t', '\r', '\n']);
            if next.len() == trim_ws.len() {
                break;
            }
            trim_ws = next;
        }
    }
    let after_begin = trim_ws.strip_prefix("*** Begin Patch")?;
    let rest = if let Some(r) = after_begin.strip_prefix("\r\n") {
        r
    } else if let Some(r) = after_begin.strip_prefix('\n') {
        r
    } else if let Some(r) = after_begin.strip_prefix("\\r\\n") {
        r
    } else {
        after_begin.strip_prefix("\\n")?
    };
    Some(src.len().saturating_sub(rest.len()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum V4aTemplateProbe {
    NotSingleV4a,
    NoEndPatchInRemainder,
    DepthExceeded,
    BrokenV4a(usize),
}

fn find_v4a_template_bounds(
    src: &str,
    open_tick_pos: usize,
    outer_delims: &[char],
) -> V4aTemplateProbe {
    let Some(content_after_header) = skip_v4a_begin_patch_header(src, open_tick_pos) else {
        return V4aTemplateProbe::NotSingleV4a;
    };

    let mut search_from = content_after_header;
    while let Some(rem) = src.get(search_from..) {
        let Some(rel) = rem.find("*** End Patch") else {
            return V4aTemplateProbe::NoEndPatchInRemainder;
        };
        let marker_pos = search_from.saturating_add(rel);
        let marker_end = marker_pos.saturating_add("*** End Patch".len());
        let line_leading = src
            .get(..marker_pos)
            .is_some_and(|prefix| prefix.ends_with('\n') || prefix.ends_with("\\n"));
        if line_leading && let Some(mut tail) = src.get(marker_end..) {
            let mut consumed = 0_usize;
            loop {
                if let Some(r) = tail.strip_prefix("\\n") {
                    tail = r;
                    consumed = consumed.saturating_add(2);
                } else if let Some(r) = tail.strip_prefix("\\r") {
                    tail = r;
                    consumed = consumed.saturating_add(2);
                } else if let Some(ch) = tail.chars().next()
                    && matches!(ch, ' ' | '\t' | '\r' | '\n')
                {
                    tail = tail.get(ch.len_utf8()..).unwrap_or_default();
                    consumed = consumed.saturating_add(ch.len_utf8());
                } else {
                    break;
                }
            }
            if tail.starts_with('`') {
                let close_tick_pos = marker_end.saturating_add(consumed);
                let Some(between) = src.get(content_after_header..marker_pos) else {
                    return V4aTemplateProbe::NotSingleV4a;
                };
                if !is_plausible_v4a_body(between) {
                    return V4aTemplateProbe::NotSingleV4a;
                }
                let Some(full_span) = src.get(open_tick_pos..=close_tick_pos) else {
                    return V4aTemplateProbe::NotSingleV4a;
                };
                return match is_valid_js_patch_expression(full_span, outer_delims) {
                    PatchExprValidity::Valid => V4aTemplateProbe::NotSingleV4a,
                    PatchExprValidity::DepthExceeded => V4aTemplateProbe::DepthExceeded,
                    PatchExprValidity::Invalid => V4aTemplateProbe::BrokenV4a(close_tick_pos),
                };
            }
        }
        search_from = marker_end;
    }
    V4aTemplateProbe::NoEndPatchInRemainder
}

fn is_plausible_v4a_body(body: &str) -> bool {
    let mut saw_file_op = false;
    for raw_line in body.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        for seg in line.split("\\n") {
            let seg = seg.strip_suffix("\\r").unwrap_or(seg);
            if seg.is_empty() {
                continue;
            }
            if seg.starts_with("*** Update File:")
                || seg.starts_with("*** Add File:")
                || seg.starts_with("*** Delete File:")
            {
                saw_file_op = true;
                continue;
            }
            if seg.starts_with("*** Move to:")
                || seg.starts_with("*** End of File")
                || seg.starts_with("@@")
                || seg.starts_with(['+', '-', ' '])
            {
                continue;
            }
            return false;
        }
    }
    saw_file_op
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatchSliceToken {
    Start,
    UntaggedTemplate,
    QuotedString,
    Ident {
        is_raw: bool,
        allows_regex: bool,
        can_precede_ident: bool,
    },
    Number,
    CloseGroup,
    Dot,
    Operator,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatchExprValidity {
    Valid,
    Invalid,
    DepthExceeded,
}

fn is_valid_js_patch_expression(slice: &str, outer_delims: &[char]) -> PatchExprValidity {
    let mut pos = 0_usize;
    let mut prev = PatchSliceToken::Start;
    let mut saw_newline = false;
    let mut delim_stack: Vec<char> = outer_delims.to_vec();
    let mut min_delim_depth = delim_stack.len();

    while let Some(tail) = src_tail(slice, pos)
        && let Some(ch) = tail.chars().next()
    {
        if matches!(ch, ' ' | '\t') {
            pos = pos.saturating_add(1);
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
            saw_newline = true;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if tail.starts_with("//") {
            pos = tail
                .find('\n')
                .map_or(slice.len(), |nl| pos.saturating_add(nl + 1));
            saw_newline = true;
            continue;
        }
        if tail.starts_with("/*") {
            let Some(rel_end) = slice
                .get(pos.saturating_add(2)..)
                .and_then(|r| r.find("*/"))
            else {
                return PatchExprValidity::Invalid;
            };
            let comment_body = slice
                .get(pos.saturating_add(2)..pos.saturating_add(2 + rel_end))
                .unwrap_or_default();
            if comment_body.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                saw_newline = true;
            }
            pos = pos.saturating_add(rel_end + 4);
            continue;
        }

        if prev == PatchSliceToken::UntaggedTemplate
            && !is_allowed_after_patch_template(tail, saw_newline)
        {
            return PatchExprValidity::Invalid;
        }

        match ch {
            '`' => {
                match prev {
                    PatchSliceToken::UntaggedTemplate
                    | PatchSliceToken::QuotedString
                    | PatchSliceToken::Number
                    | PatchSliceToken::CloseGroup
                    | PatchSliceToken::Dot => return PatchExprValidity::Invalid,
                    PatchSliceToken::Ident { is_raw: false, .. } => {
                        return PatchExprValidity::Invalid;
                    }
                    PatchSliceToken::Start
                    | PatchSliceToken::Operator
                    | PatchSliceToken::Ident { is_raw: true, .. } => {}
                }
                match skip_js_template_literal_checked(slice, pos) {
                    JsScanOutcome::Closed(end) => pos = end,
                    JsScanOutcome::Unclosed(_) => return PatchExprValidity::Invalid,
                    JsScanOutcome::DepthExceeded => return PatchExprValidity::DepthExceeded,
                }
                prev = PatchSliceToken::UntaggedTemplate;
                saw_newline = false;
            }
            '\'' | '"' => {
                if !saw_newline
                    && matches!(
                        prev,
                        PatchSliceToken::Ident {
                            can_precede_ident: false,
                            ..
                        } | PatchSliceToken::Number
                            | PatchSliceToken::QuotedString
                            | PatchSliceToken::CloseGroup
                    )
                {
                    return PatchExprValidity::Invalid;
                }
                let (end, closed) = skip_js_quoted_string_checked(slice, pos, ch);
                if !closed {
                    return PatchExprValidity::Invalid;
                }
                pos = end;
                prev = PatchSliceToken::QuotedString;
                saw_newline = false;
            }
            '/' => {
                let regex_allowed = matches!(
                    prev,
                    PatchSliceToken::Start
                        | PatchSliceToken::Operator
                        | PatchSliceToken::Ident {
                            allows_regex: true,
                            ..
                        }
                );
                if regex_allowed && let Some(end) = try_skip_js_regex_literal(slice, pos) {
                    pos = end;
                    prev = PatchSliceToken::CloseGroup;
                    saw_newline = false;
                } else if matches!(
                    prev,
                    PatchSliceToken::Start | PatchSliceToken::Operator | PatchSliceToken::Dot
                ) {
                    return PatchExprValidity::Invalid;
                } else {
                    pos = pos.saturating_add(if tail.starts_with("/=") { 2 } else { 1 });
                    prev = PatchSliceToken::Operator;
                    saw_newline = false;
                }
            }
            '(' | '[' | '{' => {
                delim_stack.push(ch);
                pos = pos.saturating_add(1);
                prev = PatchSliceToken::Operator;
                saw_newline = false;
            }
            ')' | ']' | '}' => {
                let expected = match ch {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if delim_stack.last() != Some(&expected) {
                    return PatchExprValidity::Invalid;
                }
                delim_stack.pop();
                if delim_stack.len() < min_delim_depth {
                    min_delim_depth = delim_stack.len();
                }
                pos = pos.saturating_add(1);
                prev = if ch == '}' {
                    PatchSliceToken::Operator
                } else {
                    PatchSliceToken::CloseGroup
                };
                saw_newline = false;
            }
            _ if is_js_ident_start(ch) => {
                if !saw_newline
                    && matches!(
                        prev,
                        PatchSliceToken::Ident {
                            can_precede_ident: false,
                            ..
                        } | PatchSliceToken::Number
                            | PatchSliceToken::QuotedString
                            | PatchSliceToken::CloseGroup
                    )
                {
                    return PatchExprValidity::Invalid;
                }
                let (word, end) = scan_js_ident(slice, pos);
                pos = end;
                prev = PatchSliceToken::Ident {
                    is_raw: word == "raw" && prev == PatchSliceToken::Dot,
                    allows_regex: js_keyword_allows_regex(word),
                    can_precede_ident: js_keyword_can_precede_ident(word),
                };
                saw_newline = false;
            }
            _ if ch.is_ascii_digit() => {
                if !saw_newline
                    && matches!(
                        prev,
                        PatchSliceToken::Ident {
                            can_precede_ident: false,
                            ..
                        } | PatchSliceToken::Number
                            | PatchSliceToken::QuotedString
                            | PatchSliceToken::CloseGroup
                    )
                {
                    return PatchExprValidity::Invalid;
                }
                pos = skip_js_number_literal(slice, pos);
                prev = PatchSliceToken::Number;
                saw_newline = false;
            }
            '?' if tail.starts_with("?.")
                && !tail
                    .get(2..)
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())) =>
            {
                pos = pos.saturating_add(2);
                prev = PatchSliceToken::Dot;
                saw_newline = false;
            }
            '.' => {
                if tail.starts_with("...") {
                    pos = pos.saturating_add(3);
                    prev = PatchSliceToken::Operator;
                } else {
                    pos = pos.saturating_add(1);
                    prev = PatchSliceToken::Dot;
                }
                saw_newline = false;
            }
            '+' | '-' if tail.starts_with("++") || tail.starts_with("--") => {
                pos = pos.saturating_add(2);
                saw_newline = false;
            }
            _ => {
                let Some(op_len) = scan_js_operator_len(tail).or_else(|| {
                    (ch == ';' && !matches!(delim_stack.last(), Some('(' | '['))).then_some(1)
                }) else {
                    return PatchExprValidity::Invalid;
                };
                pos = pos.saturating_add(op_len);
                prev = PatchSliceToken::Operator;
                saw_newline = false;
            }
        }
    }

    if prev == PatchSliceToken::UntaggedTemplate
        && delim_stack
            .get(min_delim_depth..)
            .unwrap_or_default()
            .iter()
            .all(|&d| d == '(' || d == '[')
    {
        PatchExprValidity::Valid
    } else {
        PatchExprValidity::Invalid
    }
}

fn src_tail(src: &str, pos: usize) -> Option<&str> {
    src.get(pos..).filter(|s| !s.is_empty())
}

fn is_allowed_after_patch_template(tail: &str, saw_newline: bool) -> bool {
    if (tail.starts_with('+') && !tail.starts_with("++") && !tail.starts_with("+-"))
        || tail.starts_with([',', ';', ')', ']', '}', '.', '?', ':'])
        || tail.starts_with("||")
        || tail.starts_with("&&")
        || tail.starts_with("==")
        || tail.starts_with("!=")
    {
        return true;
    }
    if saw_newline && let Some(ch) = tail.chars().next() {
        return is_js_ident_start(ch);
    }
    false
}

fn heal_v4a_template_backticks(src: &str) -> Cow<'_, str> {
    let mut out: Option<String> = None;
    let mut copied_until = 0_usize;
    let mut pos = 0_usize;
    let mut regex_allowed = true;
    let mut prev_can_tag_template = false;
    let mut outer_delims: Vec<char> = Vec::new();
    let mut end_patch_exhausted = false;

    while let Some(tail) = src.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        match ch {
            ' ' | '\t' | '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                pos = pos.saturating_add(ch.len_utf8());
            }
            '\'' | '"' => {
                pos = skip_js_quoted_string_checked(src, pos, ch).0;
                regex_allowed = false;
                prev_can_tag_template = false;
            }
            '/' if tail.starts_with("//") => {
                pos = tail
                    .find('\n')
                    .map_or(src.len(), |nl| pos.saturating_add(nl + 1));
            }
            '/' if tail.starts_with("/*") => {
                pos = src
                    .get(pos.saturating_add(2)..)
                    .and_then(|r| r.find("*/").map(|end| pos.saturating_add(end + 4)))
                    .unwrap_or(src.len());
            }
            '/' if regex_allowed => {
                if let Some(end) = try_skip_js_regex_literal(src, pos) {
                    pos = end;
                    regex_allowed = false;
                    prev_can_tag_template = false;
                } else {
                    pos = pos.saturating_add(1);
                    regex_allowed = true;
                    prev_can_tag_template = false;
                }
            }
            '`' => {
                let probe = if !prev_can_tag_template && !end_patch_exhausted {
                    find_v4a_template_bounds(src, pos, &outer_delims)
                } else {
                    V4aTemplateProbe::NotSingleV4a
                };
                match probe {
                    V4aTemplateProbe::DepthExceeded => return Cow::Borrowed(src),
                    V4aTemplateProbe::BrokenV4a(close_tick_pos) => {
                        let body_start = pos.saturating_add(1);
                        if let Some(body) = src.get(body_start..close_tick_pos) {
                            match escape_unescaped_template_backticks(body) {
                                TemplateHealOutcome::DepthExceeded => {
                                    return Cow::Borrowed(src);
                                }
                                TemplateHealOutcome::Healed(healed_body) => {
                                    let buf = out.get_or_insert_with(|| {
                                        String::with_capacity(src.len() + 16)
                                    });
                                    if let Some(prefix) = src.get(copied_until..body_start) {
                                        buf.push_str(prefix);
                                    }
                                    buf.push_str(&healed_body);
                                    buf.push('`');
                                    copied_until = close_tick_pos.saturating_add(1);
                                }
                                TemplateHealOutcome::Unchanged => {}
                            }
                        }
                        pos = close_tick_pos.saturating_add(1);
                    }
                    V4aTemplateProbe::NoEndPatchInRemainder | V4aTemplateProbe::NotSingleV4a => {
                        if probe == V4aTemplateProbe::NoEndPatchInRemainder {
                            end_patch_exhausted = true;
                            if out.is_none() {
                                break;
                            }
                        }
                        match skip_js_template_literal_checked(src, pos) {
                            JsScanOutcome::Closed(end) | JsScanOutcome::Unclosed(end) => pos = end,
                            JsScanOutcome::DepthExceeded => {
                                return Cow::Borrowed(src);
                            }
                        }
                    }
                }
                regex_allowed = false;
                prev_can_tag_template = false;
            }
            '(' | '[' | '{' => {
                outer_delims.push(ch);
                pos = pos.saturating_add(1);
                regex_allowed = true;
                prev_can_tag_template = false;
            }
            '}' => {
                if outer_delims.last() == Some(&'{') {
                    outer_delims.pop();
                }
                pos = pos.saturating_add(1);
                regex_allowed = true;
                prev_can_tag_template = false;
            }
            ')' | ']' => {
                let expected = if ch == ')' { '(' } else { '[' };
                if outer_delims.last() == Some(&expected) {
                    outer_delims.pop();
                }
                pos = pos.saturating_add(1);
                regex_allowed = false;
                prev_can_tag_template = true;
            }
            '+' | '-' if tail.starts_with("++") || tail.starts_with("--") => {
                pos = pos.saturating_add(2);
                prev_can_tag_template = false;
            }
            _ if is_js_ident_start(ch) => {
                let (word, end) = scan_js_ident(src, pos);
                pos = end;
                regex_allowed = js_keyword_allows_regex(word);
                prev_can_tag_template = !js_keyword_allows_regex(word);
            }
            _ if ch.is_ascii_digit() => {
                pos = skip_js_number_literal(src, pos);
                regex_allowed = false;
                prev_can_tag_template = false;
            }
            _ => {
                pos = pos.saturating_add(ch.len_utf8());
                regex_allowed = true;
                prev_can_tag_template = false;
            }
        }
    }

    if let Some(mut buf) = out {
        if let Some(rest) = src.get(copied_until..) {
            buf.push_str(rest);
        }
        Cow::Owned(buf)
    } else {
        Cow::Borrowed(src)
    }
}

enum TemplateHealOutcome {
    Unchanged,
    Healed(String),
    DepthExceeded,
}

fn escape_unescaped_template_backticks(body: &str) -> TemplateHealOutcome {
    let mut out: Option<String> = None;
    let mut copied_until = 0_usize;
    let mut pos = 0_usize;
    let mut escaped = false;

    while let Some(tail) = body.get(pos..)
        && let Some(ch) = tail.chars().next()
    {
        if escaped {
            escaped = false;
            pos = pos.saturating_add(ch.len_utf8());
            continue;
        }
        if ch == '\\' {
            escaped = true;
            pos = pos.saturating_add(1);
            continue;
        }
        if ch == '$'
            && body
                .get(pos.saturating_add(1)..)
                .is_some_and(|r| r.starts_with('{'))
        {
            match skip_js_brace_expr_checked(body, pos.saturating_add(2)) {
                JsScanOutcome::Closed(after_brace) | JsScanOutcome::Unclosed(after_brace) => {
                    pos = after_brace;
                }
                JsScanOutcome::DepthExceeded => return TemplateHealOutcome::DepthExceeded,
            }
            continue;
        }
        if ch == '`' {
            let buf = out.get_or_insert_with(|| String::with_capacity(body.len() + 8));
            if let Some(prefix) = body.get(copied_until..pos) {
                buf.push_str(prefix);
            }
            buf.push_str("\\`");
            pos = pos.saturating_add(1);
            copied_until = pos;
            continue;
        }
        pos = pos.saturating_add(ch.len_utf8());
    }

    if let Some(mut buf) = out {
        if let Some(rest) = body.get(copied_until..) {
            buf.push_str(rest);
        }
        TemplateHealOutcome::Healed(buf)
    } else {
        TemplateHealOutcome::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_code_mode_exec_js_heals_v4a_templates_and_fences() {
        let plain =
            "const r = await tools.exec_command({ cmd: `echo ${1 + 1}` });\ntext(r.output);\n";
        assert!(matches!(
            normalize_code_mode_exec_js(plain),
            Cow::Borrowed(_)
        ));

        let fenced = "```javascript\ntext('ok');\n```";
        assert_eq!(normalize_code_mode_exec_js(fenced).as_ref(), "text('ok');");

        let broken = "const patches = [\n  `*** Begin Patch\n*** Update File: ${wt}/CHANGELOG.md\n@@\n+- **Codex `exec` `JSON.parse` fix**: keep \\`escaped\\` intact\n*** End Patch`,\n  `*** Begin Patch\n*** Update File: ${wt}/README.md\n@@\n+- Run `lg cx`\n*** End Patch`\n];\nawait Promise.allSettled(patches.map(p => tools.apply_patch(p)));";
        let expected = "const patches = [\n  `*** Begin Patch\n*** Update File: ${wt}/CHANGELOG.md\n@@\n+- **Codex \\`exec\\` \\`JSON.parse\\` fix**: keep \\`escaped\\` intact\n*** End Patch`,\n  `*** Begin Patch\n*** Update File: ${wt}/README.md\n@@\n+- Run \\`lg cx\\`\n*** End Patch`\n];\nawait Promise.allSettled(patches.map(p => tools.apply_patch(p)));";
        let healed = normalize_code_mode_exec_js(broken);
        assert_eq!(healed.as_ref(), expected);
        assert!(matches!(
            normalize_code_mode_exec_js(expected),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn heals_unescaped_backticks_in_python_heredoc_and_rust_comments_without_touching_interpolations()
     {
        // Exact production failure: `SyntaxError: Unexpected identifier 'lg'`
        let broken_py = concat!(
            "const pyScript = `\n",
            "from pathlib import Path\n",
            "adapter = \"\"\"// Dedicated Auto-Update Manager TUI (`lg auto-update` / Cockpit `[v]` / Top `u`)\n",
            "#[derive(Clone)]\n",
            "struct CoreAutoUpdateTuiController { state_paths: StatePaths }\n",
            "\"\"\"\n",
            "`;\n",
            "const pyRes = await tools.exec_command({\n",
            "  cmd: `python3 - <<'PY'\\n${pyScript}\\nPY`,\n",
            "  workdir: '/tmp'\n",
            "});\n",
            "text(pyRes.output);\n",
        );
        let expected_py = concat!(
            "const pyScript = `\n",
            "from pathlib import Path\n",
            "adapter = \"\"\"// Dedicated Auto-Update Manager TUI (\\`lg auto-update\\` / Cockpit \\`[v]\\` / Top \\`u\\`)\n",
            "#[derive(Clone)]\n",
            "struct CoreAutoUpdateTuiController { state_paths: StatePaths }\n",
            "\"\"\"\n",
            "`;\n",
            "const pyRes = await tools.exec_command({\n",
            "  cmd: `python3 - <<'PY'\\n${pyScript}\\nPY`,\n",
            "  workdir: '/tmp'\n",
            "});\n",
            "text(pyRes.output);\n",
        );
        let healed = normalize_code_mode_exec_js(broken_py);
        assert_eq!(healed.as_ref(), expected_py);
        // Idempotent once healed.
        assert!(matches!(
            normalize_code_mode_exec_js(expected_py),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn heals_multiple_sequential_broken_templates_and_semicolon_terminated_inner_code() {
        let multi = concat!(
            "const a = `run `lg up` first`;\n",
            "const b = `then run `lg status` next`;\n",
            "text(a + b);\n",
        );
        let expected_multi = concat!(
            "const a = `run \\`lg up\\` first`;\n",
            "const b = `then run \\`lg status\\` next`;\n",
            "text(a + b);\n",
        );
        assert_eq!(normalize_code_mode_exec_js(multi).as_ref(), expected_multi);

        let with_semis = concat!(
            "const rs1 = `\nlet x = `foo`;\nlet y = 2;\n`;\n",
            "const rs2 = `\nlet z = `bar`;\n`;\n",
            "text(rs1 + rs2);\n",
        );
        let expected_semis = concat!(
            "const rs1 = `\nlet x = \\`foo\\`;\nlet y = 2;\n`;\n",
            "const rs2 = `\nlet z = \\`bar\\`;\n`;\n",
            "text(rs1 + rs2);\n",
        );
        assert_eq!(
            normalize_code_mode_exec_js(with_semis).as_ref(),
            expected_semis
        );
    }

    #[test]
    fn preserves_valid_tagged_templates_control_flow_and_asi_newlines_untouched() {
        let valid_scripts = [
            "if (ok) run();\nconst r = String.raw`keep \\` and ${1 + 2}`;\ntext(r);\n",
            "const a = `line 1`\nconst b = `line 2`\ntext(a + b)\n",
            "const ok = `item` in obj && `x` instanceof String;\ntext(ok);\n",
            "const msg = 'mention btoa in string only';\n// btoa in comment\ntext(msg);\n",
        ];
        for script in valid_scripts {
            assert!(
                matches!(normalize_code_mode_exec_js(script), Cow::Borrowed(_)),
                "valid script must remain Cow::Borrowed: {script}"
            );
        }
    }

    #[test]
    fn injects_btoa_atob_shim_after_exec_pragma_and_stays_idempotent() {
        let with_pragma =
            "// @exec: {\"yield_time_ms\": 30000}\nconst b64 = btoa('hello');\ntext(atob(b64));\n";
        let normalized = normalize_code_mode_exec_js(with_pragma);
        assert!(
            normalized.starts_with("// @exec: {\"yield_time_ms\": 30000}\nglobalThis.btoa ??="),
            "first-line // @exec: pragma must stay on line 1: {normalized}"
        );
        assert!(normalized.ends_with("const b64 = btoa('hello');\ntext(atob(b64));\n"));
        assert!(matches!(
            normalize_code_mode_exec_js(&normalized),
            Cow::Borrowed(_)
        ));

        let in_interp = "const url = `data:text/plain;base64,${btoa('hi')}`;\ntext(url);\n";
        let norm_interp = normalize_code_mode_exec_js(in_interp);
        assert!(norm_interp.starts_with("globalThis.btoa ??="));
    }
}
