use crate::acp::AvailableModel;
use crate::canonical::ReasoningEffort;
use crate::config::ModelId;

/// Delimiter separating the Claude Desktop safe role prefix (`claude-{opus|sonnet|haiku}-5-5`)
/// from the reversibly encoded Antigravity ACP model identifier.
pub const CLAUDE_DESKTOP_ROUTE_MARKER: &str = "-lg--";

const GEMINI_DEFAULT_WINDOW: u64 = 280_000;
const GEMINI_DEFAULT_COMPACT_LIMIT: u64 = 245_000;
const GEMINI_LONG_WINDOW: u64 = 1_000_000;
const GEMINI_LONG_COMPACT_LIMIT: u64 = 850_000;
const CLAUDE_DEFAULT_WINDOW: u64 = 200_000;
const CLAUDE_DEFAULT_COMPACT_LIMIT: u64 = 170_000;
const GPT_OSS_DEFAULT_WINDOW: u64 = 128_000;
const GPT_OSS_DEFAULT_COMPACT_LIMIT: u64 = 110_000;

const _: () = {
    assert!(
        GEMINI_DEFAULT_WINDOW > GEMINI_DEFAULT_COMPACT_LIMIT,
        "default Gemini window must exceed its auto-compaction threshold"
    );
    assert!(
        GEMINI_LONG_WINDOW > GEMINI_LONG_COMPACT_LIMIT
            && GEMINI_LONG_COMPACT_LIMIT > GEMINI_DEFAULT_WINDOW,
        "experimental Gemini long-context limits must exceed default limits"
    );
    assert!(
        CLAUDE_DEFAULT_WINDOW > CLAUDE_DEFAULT_COMPACT_LIMIT,
        "Claude window must exceed its auto-compaction threshold"
    );
    assert!(
        GPT_OSS_DEFAULT_WINDOW > GPT_OSS_DEFAULT_COMPACT_LIMIT
            && GPT_OSS_DEFAULT_COMPACT_LIMIT >= 100_000,
        "GPT-OSS window must exceed its auto-compaction threshold and stay >= 100k"
    );
};

/// Advertised context window and client compaction threshold for the ACP transport.
/// The experimental Gemini window does not disable upstream automatic compaction.
#[must_use]
pub fn context_limits(model_id: &str, long_context: bool) -> (u64, u64) {
    let trimmed = model_id.trim();
    if trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("gemini-"))
    {
        if long_context {
            (GEMINI_LONG_WINDOW, GEMINI_LONG_COMPACT_LIMIT)
        } else {
            (GEMINI_DEFAULT_WINDOW, GEMINI_DEFAULT_COMPACT_LIMIT)
        }
    } else if trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("gpt-oss"))
    {
        (GPT_OSS_DEFAULT_WINDOW, GPT_OSS_DEFAULT_COMPACT_LIMIT)
    } else {
        (CLAUDE_DEFAULT_WINDOW, CLAUDE_DEFAULT_COMPACT_LIMIT)
    }
}

/// Resolves an arbitrary caller model string (from Codex, Claude Code, or direct ACP model id)
/// and optional reasoning effort into a concrete ACP `ModelId`.
#[must_use]
pub fn resolve_model_id(
    requested: Option<&str>,
    effort_hint: Option<ReasoningEffort>,
    available: &[AvailableModel],
    default_model: &ModelId,
) -> ModelId {
    let Some(raw_input) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return apply_effort_to_base(default_model.as_str(), effort_hint, available)
            .unwrap_or_else(|| default_model.clone());
    };
    let raw_stripped = strip_context_window_tag(raw_input);
    let decoded_desktop = decode_claude_desktop_route_id(raw_stripped);
    let raw = decoded_desktop.as_deref().unwrap_or(raw_stripped);

    // 1. Exact match in ACP catalog — except when caller kept a `-high` default model while
    //    explicitly lowering `--effort` (`low` or `medium`). Never upgrade `-high` -> `-xhigh`
    //    or `-low` -> `-high` when an exact model ID was requested (Codex defaults unknown
    //    third-party models to `reasoning.effort = "xhigh"`).
    let caller_lowered_high_default = raw.to_ascii_lowercase().ends_with("-high")
        && matches!(
            effort_hint,
            Some(ReasoningEffort::Low | ReasoningEffort::Medium)
        );
    if !caller_lowered_high_default
        && let Some(hit) = available
            .iter()
            .find(|m| m.model_id.eq_ignore_ascii_case(raw))
        && let Ok(id) = ModelId::try_new(&hit.model_id)
    {
        return id;
    }

    // 2. Prefix + effort match (e.g. `gemini-4-carbon` or `claude-sonnet-5-5` + effort)
    if let Some(id) = apply_effort_to_base(raw, effort_hint, available) {
        return id;
    }

    // 3. Smart family mapping for Claude Code & OpenAI Codex harnesses
    let lower = raw.to_ascii_lowercase();
    if lower.contains("haiku")
        || lower.contains("flash")
        || lower.contains("mini")
        || lower.contains("nano")
    {
        return select_fastest_gemini_model(available, default_model);
    } else if lower.contains("opus") {
        if let Some(id) = select_claude_family_model("opus", effort_hint, available) {
            return id;
        }
    } else if lower.contains("sonnet") || lower.contains("fable") {
        if let Some(id) = select_claude_family_model("sonnet", effort_hint, available)
            .or_else(|| select_claude_family_model("opus", effort_hint, available))
        {
            return id;
        }
    } else if lower.contains("orchestrator") {
        let preferred = if lower.contains("argon") {
            "gemini-pro-orchestrator-argon"
        } else {
            "gemini-pro-orchestrator"
        };
        if let Some(hit) = available
            .iter()
            .find(|m| m.model_id.eq_ignore_ascii_case(preferred))
            .or_else(|| {
                available.iter().find(|m| {
                    m.model_id
                        .to_ascii_lowercase()
                        .starts_with("gemini-pro-orchestrator")
                })
            })
            && let Ok(id) = ModelId::try_new(&hit.model_id)
        {
            return id;
        }
    }

    // 4. If available catalog is empty (e.g. unit test without catalog) and raw is a valid Antigravity/Claude/Gemini id, pass it through
    if available.is_empty()
        && (lower.starts_with("gemini-")
            || lower.starts_with("claude-")
            || lower.starts_with("gpt-oss"))
        && let Ok(id) = ModelId::try_new(raw)
    {
        return id;
    }

    // 5. Fallback to default model (applying caller's effort hint if possible)
    apply_effort_to_base(default_model.as_str(), effort_hint, available)
        .unwrap_or_else(|| default_model.clone())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FastGeminiRank {
    family_tier: u8,
    effort_score: u8,
    major: u32,
    minor: u32,
}

/// Dynamically selects the newest, fastest, lowest-cost Gemini model from the live ACP catalog
/// for Auto Mode safety classification and lightweight background utility calls.
#[must_use]
pub fn select_fastest_gemini_model(available: &[AvailableModel], fallback: &ModelId) -> ModelId {
    let mut best: Option<(FastGeminiRank, &str)> = None;

    for candidate in available {
        let id = candidate.model_id.trim();
        let lower = id.to_ascii_lowercase();
        if !lower.starts_with("gemini-") {
            continue;
        }

        let family_tier: u8 = if lower.contains("flash-lite") {
            5
        } else if lower.contains("flash") {
            4
        } else if lower.contains("argon") {
            2
        } else if lower.contains("carbon") {
            1
        } else {
            continue;
        };

        let effort_score: u8 = if lower.ends_with("-low") {
            4
        } else if lower.ends_with("-medium") {
            3
        } else if !lower.ends_with("-high") && !lower.ends_with("-xhigh") {
            2
        } else {
            1
        };

        let (major, minor) = parse_gemini_version(&lower);
        // Prefer low-effort Flash/Flash-Lite first, then newest Gemini generation
        let rank = FastGeminiRank {
            family_tier,
            effort_score,
            major,
            minor,
        };
        if best.as_ref().is_none_or(|(prev_rank, _)| rank > *prev_rank) {
            best = Some((rank, id));
        }
    }

    if let Some((_, chosen)) = best
        && let Ok(model_id) = ModelId::try_new(chosen)
    {
        return model_id;
    }

    ModelId::try_new("gemini-3.8-flash-low").unwrap_or_else(|_| fallback.clone())
}

fn parse_gemini_version(lower_id: &str) -> (u32, u32) {
    let Some(rest) = lower_id.strip_prefix("gemini-") else {
        return (0, 0);
    };
    let ver_token = rest.split('-').next().unwrap_or_default();
    if let Some((maj_s, min_s)) = ver_token.split_once('.') {
        let maj = maj_s.parse::<u32>().unwrap_or(0);
        let min = min_s.parse::<u32>().unwrap_or(0);
        (maj, min)
    } else {
        (ver_token.parse::<u32>().unwrap_or(0), 0)
    }
}

fn select_claude_family_model(
    family: &str,
    effort_hint: Option<ReasoningEffort>,
    available: &[AvailableModel],
) -> Option<ModelId> {
    let prefix = format!("claude-{family}-");
    let mut best_version: Option<((u32, u32), &str)> = None;

    for candidate in available {
        let id = candidate.model_id.trim();
        let lower = id.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix(&prefix) else {
            continue;
        };
        let mut parts = rest.split('-');
        let Some(first) = parts.next() else {
            continue;
        };
        let (version, base_len) = if let Some((maj_s, min_s)) = first.split_once('.')
            && let Ok(maj) = maj_s.parse::<u32>()
            && let Ok(min) = min_s.parse::<u32>()
        {
            ((maj, min), prefix.len() + first.len())
        } else if let Ok(maj) = first.parse::<u32>() {
            if let Some(second) = parts.next()
                && let Ok(min) = second.parse::<u32>()
                && min < 1_000
            {
                ((maj, min), prefix.len() + first.len() + 1 + second.len())
            } else {
                ((maj, 0), prefix.len() + first.len())
            }
        } else {
            ((0, 0), id.len())
        };

        let Some(base) = id.get(..base_len) else {
            continue;
        };
        if best_version.is_none_or(|(prev_ver, _)| version > prev_ver) {
            best_version = Some((version, base));
        }
    }

    let (_, best_base) = best_version?;
    if let Some(id) = apply_effort_to_base(
        best_base,
        Some(effort_hint.unwrap_or(ReasoningEffort::High)),
        available,
    ) {
        return Some(id);
    }

    let matches_best_base = |m: &&AvailableModel| {
        let trimmed = m.model_id.trim();
        trimmed
            .get(..best_base.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(best_base))
            && trimmed
                .as_bytes()
                .get(best_base.len())
                .is_none_or(|&next| next == b'-')
    };

    let chosen = if effort_hint != Some(ReasoningEffort::Low) {
        available
            .iter()
            .filter(matches_best_base)
            .find(|m| m.model_id.to_ascii_lowercase().ends_with("-thinking"))
            .or_else(|| available.iter().find(matches_best_base))
    } else {
        available.iter().find(matches_best_base)
    }?;

    ModelId::try_new(&chosen.model_id).ok()
}

fn apply_effort_to_base(
    base_or_full: &str,
    effort: Option<ReasoningEffort>,
    available: &[AvailableModel],
) -> Option<ModelId> {
    let base = strip_known_effort_suffix(base_or_full);
    let target_efforts: &[&str] = match effort {
        Some(ReasoningEffort::XHigh) => &["xhigh", "high", "medium", "low"],
        Some(ReasoningEffort::High) => &["high", "medium", "low"],
        Some(ReasoningEffort::Medium) => &["medium", "high", "low"],
        Some(ReasoningEffort::Low) => &["low", "medium", "high"],
        None => &["high", "medium", "low"],
    };

    for suffix in target_efforts {
        let candidate = format!("{base}-{suffix}");
        if let Some(hit) = available
            .iter()
            .find(|m| m.model_id.eq_ignore_ascii_case(&candidate))
            && let Ok(id) = ModelId::try_new(&hit.model_id)
        {
            return Some(id);
        }
    }

    available
        .iter()
        .find(|m| m.model_id.eq_ignore_ascii_case(base))
        .and_then(|hit| ModelId::try_new(&hit.model_id).ok())
}

fn strip_known_effort_suffix(model: &str) -> &str {
    for suffix in ["-xhigh", "-high", "-medium", "-low"] {
        if let Some(split_at) = model.len().checked_sub(suffix.len())
            && let Some((head, tail)) = model.split_at_checked(split_at)
            && tail.eq_ignore_ascii_case(suffix)
        {
            return head;
        }
    }
    model
}

fn strip_context_window_tag(model: &str) -> &str {
    for suffix in [
        "[1m]", "[1M]", "[280k]", "[280K]", "[200k]", "[200K]", "[128k]", "[128K]",
    ] {
        if let Some(stripped) = model.strip_suffix(suffix) {
            return stripped.trim();
        }
    }
    model
}

/// Returns `true` when `model_id` satisfies Claude Desktop's 3P `inferenceModels` validator
/// (`claude-{sonnet|opus|haiku|fable}-<non-empty>` without `[1m]` tags).
#[must_use]
pub fn is_claude_desktop_safe_model_id(model_id: &str) -> bool {
    let normalized = model_id.trim().to_ascii_lowercase();
    if normalized.contains("[1m]") {
        return false;
    }
    let Some(tail) = normalized
        .strip_prefix("anthropic/claude-")
        .or_else(|| normalized.strip_prefix("claude-"))
    else {
        return false;
    };
    ["sonnet-", "opus-", "haiku-", "fable-"]
        .iter()
        .any(|prefix| {
            tail.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        })
}

/// Encodes an arbitrary ACP `model_id` into a lowercase `[a-z0-9-]` Claude Desktop safe route ID
/// whose prefix selects the matching 5.5 capability tier (`opus`, `sonnet`, or `haiku`).
#[must_use]
pub fn encode_claude_desktop_route_id(model_id: &str) -> String {
    let trimmed = model_id.trim();
    let lower = trimmed.to_ascii_lowercase();
    let role_prefix = if lower.contains("flash-low") || lower.contains("haiku") {
        "claude-haiku-5-5"
    } else if lower.contains("sonnet") || lower.ends_with("-medium") {
        "claude-sonnet-5-5"
    } else {
        "claude-opus-5-5"
    };
    let slug = encode_claude_desktop_slug(trimmed);
    format!("{role_prefix}{CLAUDE_DESKTOP_ROUTE_MARKER}{slug}")
}

/// Decodes a Liftgate-synthesized Claude Desktop route ID (with or without a `[1m]` suffix)
/// back into the exact upstream ACP model identifier.
#[must_use]
pub fn decode_claude_desktop_route_id(requested: &str) -> Option<String> {
    let clean = strip_context_window_tag(requested.trim());
    let lower = clean.to_ascii_lowercase();
    if !lower.starts_with("claude-") && !lower.starts_with("anthropic/claude-") {
        return None;
    }
    let marker_pos = lower.find(CLAUDE_DESKTOP_ROUTE_MARKER)?;
    let encoded_tail = clean.get(marker_pos + CLAUDE_DESKTOP_ROUTE_MARKER.len()..)?;
    if encoded_tail.is_empty() {
        return None;
    }
    decode_claude_desktop_slug(encoded_tail)
}

fn encode_claude_desktop_slug(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut idx = 0usize;
    while let Some(&b) = bytes.get(idx) {
        if b == b'-' && bytes.get(idx + 1) == Some(&b'x') {
            out.push_str("-xx");
            idx += 2;
            continue;
        }
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'-' => out.push(char::from(b)),
            b'.' => out.push_str("-xd"),
            b'_' => out.push_str("-xu"),
            b'/' => out.push_str("-xs"),
            other => {
                use core::fmt::Write as _;
                let _ = write!(out, "-xh{other:02x}");
            }
        }
        idx += 1;
    }
    out
}

fn decode_claude_desktop_slug(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while let Some(&b) = bytes.get(idx) {
        if b == b'-' && bytes.get(idx + 1) == Some(&b'x') {
            match bytes.get(idx + 2).copied() {
                Some(b'x') => {
                    out.extend_from_slice(b"-x");
                    idx += 3;
                    continue;
                }
                Some(b'd') => {
                    out.push(b'.');
                    idx += 3;
                    continue;
                }
                Some(b'u') => {
                    out.push(b'_');
                    idx += 3;
                    continue;
                }
                Some(b's') => {
                    out.push(b'/');
                    idx += 3;
                    continue;
                }
                Some(b'h') => {
                    let hi = bytes.get(idx + 3).copied().and_then(hex_nibble)?;
                    let lo = bytes.get(idx + 4).copied().and_then(hex_nibble)?;
                    out.push((hi << 4) | lo);
                    idx += 5;
                    continue;
                }
                _ => {}
            }
        }
        out.push(b);
        idx += 1;
    }
    String::from_utf8(out).ok().filter(|s| !s.is_empty())
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_catalog() -> Vec<AvailableModel> {
        vec![
            AvailableModel {
                model_id: "gemini-4-carbon-medium".to_owned(),
                name: "Gemini 4 Carbon (Medium)".to_owned(),
            },
            AvailableModel {
                model_id: "gemini-4-carbon-high".to_owned(),
                name: "Gemini 4 Carbon (High)".to_owned(),
            },
            AvailableModel {
                model_id: "gemini-3.8-flash-high".to_owned(),
                name: "Gemini 3.8 Flash (High)".to_owned(),
            },
            AvailableModel {
                model_id: "gemini-3.8-flash-low".to_owned(),
                name: "Gemini 3.8 Flash (Low)".to_owned(),
            },
            AvailableModel {
                model_id: "claude-sonnet-5-5-high".to_owned(),
                name: "Claude Sonnet 5.5 (High)".to_owned(),
            },
            AvailableModel {
                model_id: "claude-opus-5-5-high".to_owned(),
                name: "Claude Opus 5.5 (High)".to_owned(),
            },
        ]
    }

    #[test]
    fn maps_claude_code_and_codex_models_accurately() {
        let catalog = sample_catalog();
        let default = ModelId::try_new("gemini-4-carbon-medium").expect("non-empty model");

        assert_eq!(
            resolve_model_id(Some("claude-sonnet-4-5-20250929"), None, &catalog, &default).as_str(),
            "claude-sonnet-5-5-high"
        );
        assert_eq!(
            resolve_model_id(Some("claude-3-5-haiku-20241022"), None, &catalog, &default).as_str(),
            "gemini-3.8-flash-low"
        );
        assert_eq!(
            resolve_model_id(
                Some("gpt-5-codex"),
                Some(ReasoningEffort::High),
                &catalog,
                &default
            )
            .as_str(),
            "gemini-4-carbon-high"
        );
    }

    #[test]
    fn maps_claude_and_orchestrator_aliases_against_live_4_6_catalog() {
        let live_catalog = vec![
            AvailableModel {
                model_id: "gemini-4-argon-high".to_owned(),
                name: "Gemini 4 Argon (High)".to_owned(),
            },
            AvailableModel {
                model_id: "gemini-pro-orchestrator".to_owned(),
                name: "Orchestrator (Carbon)".to_owned(),
            },
            AvailableModel {
                model_id: "gemini-pro-orchestrator-argon".to_owned(),
                name: "Orchestrator (Argon)".to_owned(),
            },
            AvailableModel {
                model_id: "claude-sonnet-4-6".to_owned(),
                name: "Claude Sonnet 4.6 (Thinking)".to_owned(),
            },
            AvailableModel {
                model_id: "claude-opus-4-6-thinking".to_owned(),
                name: "Claude Opus 4.6 (Thinking)".to_owned(),
            },
        ];
        let default = ModelId::try_new("gemini-4-argon-high").expect("non-empty model");

        assert_eq!(
            resolve_model_id(
                Some("claude-sonnet-4-5-20250929"),
                None,
                &live_catalog,
                &default
            )
            .as_str(),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            resolve_model_id(Some("claude-opus-4-6"), None, &live_catalog, &default).as_str(),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            resolve_model_id(Some("claude-opus-5-5"), None, &live_catalog, &default).as_str(),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            resolve_model_id(Some("orchestrator-argon"), None, &live_catalog, &default).as_str(),
            "gemini-pro-orchestrator-argon"
        );
    }

    #[test]
    fn selects_newest_fastest_gemini_flash_low() {
        let mut catalog = sample_catalog();
        let default = ModelId::try_new("gemini-4-carbon-high").expect("non-empty model");
        assert_eq!(
            select_fastest_gemini_model(&catalog, &default).as_str(),
            "gemini-3.8-flash-low"
        );

        catalog.push(AvailableModel {
            model_id: "gemini-4.2-flash-low".to_owned(),
            name: "Gemini 4.2 Flash (Low)".to_owned(),
        });
        assert_eq!(
            select_fastest_gemini_model(&catalog, &default).as_str(),
            "gemini-4.2-flash-low"
        );
    }

    #[test]
    fn claude_desktop_route_ids_are_safe_bijective_and_resolve_with_effort() {
        let catalog = sample_catalog();
        let default = ModelId::try_new("gemini-4-carbon-high").expect("non-empty model");

        for raw in [
            "gemini-4-carbon-high",
            "gemini-4-carbon-medium",
            "gemini-3.8-flash-low",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b_v2/alpha-xtest",
        ] {
            let route_id = encode_claude_desktop_route_id(raw);
            assert!(is_claude_desktop_safe_model_id(&route_id), "{route_id}");
            assert!(!is_claude_desktop_safe_model_id(&format!("{route_id}[1m]")));
            assert_eq!(
                decode_claude_desktop_route_id(&route_id).as_deref(),
                Some(raw)
            );
            assert_eq!(
                decode_claude_desktop_route_id(&format!("{route_id}[1m]")).as_deref(),
                Some(raw)
            );
        }

        let carbon_route = encode_claude_desktop_route_id("gemini-4-carbon-high");
        assert_eq!(
            resolve_model_id(
                Some(&format!("{carbon_route}[1m]")),
                Some(ReasoningEffort::Medium),
                &catalog,
                &default,
            )
            .as_str(),
            "gemini-4-carbon-medium"
        );
        assert!(!is_claude_desktop_safe_model_id("gemini-4-carbon-high"));
        assert!(!is_claude_desktop_safe_model_id("claude-sonnet-"));
    }
}
