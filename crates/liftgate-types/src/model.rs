use crate::acp::AvailableModel;
use crate::canonical::ReasoningEffort;
use crate::config::ModelId;

/// Advertised context window and client compaction threshold for the ACP transport.
/// The experimental Gemini window does not disable upstream automatic compaction.
#[must_use]
pub fn context_limits(model_id: &str, long_context: bool) -> (u64, u64) {
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("gemini-") {
        if long_context {
            (1_000_000, 850_000)
        } else {
            (280_000, 245_000)
        }
    } else if lower.starts_with("gpt-oss") {
        (128_000, 110_000)
    } else {
        (200_000, 170_000)
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
    let raw = strip_context_window_tag(raw_input);

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
        if let Some(id) = apply_effort_to_base(
            "claude-opus-5-5",
            Some(effort_hint.unwrap_or(ReasoningEffort::High)),
            available,
        ) {
            return id;
        }
    } else if lower.contains("sonnet") {
        if let Some(id) = apply_effort_to_base(
            "claude-sonnet-5-5",
            Some(effort_hint.unwrap_or(ReasoningEffort::High)),
            available,
        ) {
            return id;
        }
    } else if lower.contains("orchestrator")
        && let Some(hit) = available
            .iter()
            .find(|m| m.model_id == "gemini-pro-orchestrator")
        && let Ok(id) = ModelId::try_new(&hit.model_id)
    {
        return id;
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
        if let Some(stripped) = model.strip_suffix(suffix) {
            return stripped;
        }
    }
    model
}

fn strip_context_window_tag(model: &str) -> &str {
    for suffix in ["[1m]", "[1M]", "[200k]", "[200K]"] {
        if let Some(stripped) = model.strip_suffix(suffix) {
            return stripped.trim();
        }
    }
    model
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
}
