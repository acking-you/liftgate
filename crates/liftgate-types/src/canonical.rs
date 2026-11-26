use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Reasoning / thinking intensity requested by the API caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    #[must_use]
    pub const fn as_suffix(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    #[must_use]
    pub fn from_effort_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" | "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "max" | "ultra" | "persistent" => Some(Self::XHigh),
            _ => None,
        }
    }

    #[must_use]
    pub const fn from_budget_tokens(budget: u64) -> Self {
        if budget >= 24_000 {
            Self::XHigh
        } else if budget >= 10_000 {
            Self::High
        } else if budget >= 3_000 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// Identifies lightweight harness background/classifier sub-requests (e.g. Claude Code Auto Mode
/// safety classifier, background away-recap, session title generator, or internal web-tool helpers)
/// so the gateway can select a model by task and close its one-shot
/// ACP session immediately without stealing or evicting interactive sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryTaskKind {
    AutoModeClassifier,
    WebSearchSubrequest,
    WebFetchExtractor,
    SessionTitleOrTopic,
    AwayRecapOrSuggestion,
}

/// Canonical role across OpenAI Responses and Anthropic Messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanonicalRole {
    User,
    Assistant,
}

/// Canonical content item within a normalized conversation message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalPart {
    Text(String),
    ImageBase64 {
        media_type: String,
        data: String,
    },
    Reasoning(String),
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
        is_custom: bool,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

impl CanonicalPart {
    /// Estimates heap bytes retained by this content part.
    #[must_use]
    pub fn estimated_heap_bytes(&self) -> usize {
        match self {
            Self::Text(s) | Self::Reasoning(s) => s.len(),
            Self::ImageBase64 { media_type, data } => media_type.len().saturating_add(data.len()),
            Self::ToolCall {
                call_id,
                name,
                arguments,
                is_custom: _,
            } => call_id
                .len()
                .saturating_add(name.len())
                .saturating_add(arguments.len()),
            Self::ToolResult {
                call_id,
                output,
                is_error: _,
            } => call_id.len().saturating_add(output.len()),
        }
    }
}

/// Single normalized conversation turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalMessage {
    pub role: CanonicalRole,
    pub parts: Vec<CanonicalPart>,
}

impl CanonicalMessage {
    /// Estimates structural plus heap bytes retained by this message.
    #[must_use]
    pub fn estimated_heap_bytes(&self) -> usize {
        let base = std::mem::size_of::<Self>().saturating_add(
            self.parts
                .len()
                .saturating_mul(std::mem::size_of::<CanonicalPart>()),
        );
        self.parts.iter().fold(base, |acc, part| {
            acc.saturating_add(part.estimated_heap_bytes())
        })
    }
}

/// Kind of client-declared tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalToolKind {
    Function { parameters_json: String },
    Custom { format_json: Option<String> },
}

/// Normalized client tool definition passed by the harness (e.g. Codex or Claude Code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub kind: CanonicalToolKind,
}

impl CanonicalToolSpec {
    /// Estimates structural plus heap bytes retained by this tool specification.
    #[must_use]
    pub fn estimated_heap_bytes(&self) -> usize {
        let kind_bytes = match &self.kind {
            CanonicalToolKind::Function { parameters_json } => parameters_json.len(),
            CanonicalToolKind::Custom { format_json } => format_json.as_deref().map_or(0, str::len),
        };
        std::mem::size_of::<Self>()
            .saturating_add(self.name.len())
            .saturating_add(self.description.as_deref().map_or(0, str::len))
            .saturating_add(kind_bytes)
    }
}

/// Markdown hyperlink span extracted from grounded assistant text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownLinkSpan {
    pub title: String,
    pub url: String,
    pub start_char: usize,
    pub end_char: usize,
}

/// Protocol-agnostic normalized turn request ready for ACP session routing.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "explicit orthogonal protocol capability flags normalized from OpenAI & Anthropic wire formats"
)]
pub struct NormalizedTurnRequest {
    pub requested_model: Option<String>,
    pub system_instructions: Option<String>,
    pub messages: Vec<CanonicalMessage>,
    pub client_tools: Vec<CanonicalToolSpec>,
    pub enable_native_web_search: bool,
    pub force_native_web_search: bool,
    pub enable_native_web_fetch: bool,
    pub force_native_web_fetch: bool,
    pub web_fetch_max_uses: Option<u32>,
    pub web_fetch_allowed_domains: Vec<String>,
    pub web_fetch_blocked_domains: Vec<String>,
    pub auxiliary_task: Option<AuxiliaryTaskKind>,
    pub previous_response_id: Option<String>,
    pub retain_response_history: bool,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub include_thinking: bool,
    pub omit_thinking_text: bool,
    pub include_encrypted_reasoning: bool,
    pub stream: bool,
}

impl NormalizedTurnRequest {
    /// Enforces the canonical ACP gateway invariant across every protocol adapter (OpenAI,
    /// Anthropic, and future harnesses such as OpenCode): drains any caller `system_instructions`
    /// into `<developer_instructions>` blocks inside `messages` so `_meta.antigravity.systemPrompt.append`
    /// never carries caller system prompts.
    pub fn materialize_system_instructions(&mut self) {
        if let Some(sys) = self.system_instructions.take() {
            crate::session_key::materialize_system_prompt_into_messages(
                Some(&sys),
                self.previous_response_id.is_some(),
                &self.client_tools,
                &mut self.messages,
            );
        }
    }
}

/// Detects whether an incoming normalized turn is a harness-internal classifier or background
/// utility call (such as Claude Code's Auto Mode Stage 1/2 safety classifier, `WebSearch` sub-call,
/// `WebFetch` HTML->Markdown extractor, away-summary recap, or session title generator).
#[must_use]
pub fn detect_auxiliary_task(
    system_instructions: Option<&str>,
    messages: &[CanonicalMessage],
    client_tools: &[CanonicalToolSpec],
    enable_native_web_search: bool,
    enable_native_web_fetch: bool,
) -> Option<AuxiliaryTaskKind> {
    let last_user_text = last_user_plain_text(messages).unwrap_or_default();
    let trimmed_user = last_user_text.trim();
    let sys = system_instructions.unwrap_or_default();
    let conv_len = count_conversational_messages(messages);

    // 1. Claude Code Auto Mode safety classifier (Stage 1 & Stage 2), startup recon,
    //    & permission classifiers. Checked unconditionally before tool checks so Stage 1,
    //    Stage 2, and startup recon use the fast auxiliary model.
    if is_auto_mode_classifier_prompt(sys, trimmed_user) {
        return Some(AuxiliaryTaskKind::AutoModeClassifier);
    }

    // 2. Forked background away-recap or speculative next-prompt suggestion
    //    (Checked before `client_tools` because Claude Code / Codex may attach session tools
    //    when forking the conversation history for background recaps.)
    if trimmed_user.starts_with("The user stepped away and is coming back. Recap in")
        || (trimmed_user.starts_with("<recap>") && trimmed_user.contains("User stepped away"))
        || trimmed_user.contains("<prompt_suggestion>")
        || sys.contains("Suggest what the user might")
        || sys.contains(
            "Summarize what was done in this conversation. Write like a pull request description.",
        )
        || sys.contains("Summarize conversation changes as a pull request description.")
        || sys.starts_with("You design the live status widget of a coding agent's background job")
        || sys.contains("Never restate the stated purpose or name the shell")
    {
        return Some(AuxiliaryTaskKind::AwayRecapOrSuggestion);
    }

    // 2b. Forked session/agent naming requests (Oh My Pi `title-fork.md` & `tan-name.md`),
    //     which branch off multi-turn conversations and may carry full session `client_tools`.
    if (trimmed_user.starts_with("<title-request>")
        && trimmed_user.contains("Session UI request, not from the user."))
        || sys.contains("Name the background agent that carries out the request in the final")
        || (client_tools.is_empty()
            && !enable_native_web_search
            && !enable_native_web_fetch
            && sys.contains("You name conversations so the user can find them later"))
    {
        return Some(AuxiliaryTaskKind::SessionTitleOrTopic);
    }

    // 3. Session title / topic / branch name generator (Claude Code, Codex, OpenCode & Oh My Pi)
    if conv_len <= 2
        && (trimmed_user.starts_with("Generate a concise, single-line task title")
            || sys.contains("Generate a concise, single-line task title")
            || trimmed_user.contains("Write the title in the predominant language of the session")
            || sys.contains("Write the title in the predominant language of the session")
            || sys.contains("isNewTopic")
            || sys.contains("You name conversations so the user can find them later")
            || sys.contains("You name coding-agent sessions.")
            || sys.contains("You head coding-agent session titles with a small card.")
            || sys.contains("Name the plan in the next user message:")
            || sys.contains("NEVER echo header names or assignment structure.")
            || sys.contains("You are a title generator")
            || (trimmed_user.starts_with("<session>")
                && trimmed_user.contains("</session>")
                && trimmed_user.contains("title")))
    {
        return Some(AuxiliaryTaskKind::SessionTitleOrTopic);
    }

    if !client_tools.is_empty() || enable_native_web_fetch {
        return None;
    }

    // OpenCode built-in hidden `summary` agent (`PROMPT_SUMMARY`), which summarizes multi-turn
    // sessions (`conv_len > 2`) with `client_tools` denied (`*`: `deny`).
    if !enable_native_web_search
        && sys.starts_with(
            "Summarize what was done in this conversation. Write like a pull request description.",
        )
    {
        return Some(AuxiliaryTaskKind::AwayRecapOrSuggestion);
    }

    if enable_native_web_search {
        if conv_len <= 1
            && (trimmed_user.starts_with("Perform a web search for the query:")
                || trimmed_user.starts_with("Perform a web search for:"))
        {
            return Some(AuxiliaryTaskKind::WebSearchSubrequest);
        }
        return None;
    }

    // 4. Claude Code `WebFetch` HTML-to-Markdown / prompt extraction sub-request
    if conv_len <= 1
        && (trimmed_user.starts_with("Web page content:\n---\n")
            || trimmed_user.starts_with("Web page content:\r\n---\r\n"))
    {
        return Some(AuxiliaryTaskKind::WebFetchExtractor);
    }

    // 5. Oh My Pi (`omp`) one-shot startup skill routing-hint compressor, commit-message
    //    generator, and background memory extractor / consolidator (`client_tools` empty above).
    if conv_len <= 2
        && (trimmed_user.starts_with("Compress into one routing hint of at most 12 words:")
            || sys.starts_with("From provided diff, generate concise git commit message.")
            || sys.starts_with("You are a precise long-term memory extractor.")
            || sys.starts_with("Summarize memories in 1-3 concise sentences."))
    {
        return Some(AuxiliaryTaskKind::AwayRecapOrSuggestion);
    }

    None
}

fn count_conversational_messages(messages: &[CanonicalMessage]) -> usize {
    messages
        .iter()
        .filter(|m| {
            !m.parts.iter().all(|p| {
                matches!(p, CanonicalPart::Text(t) if {
                    let tr = t.trim();
                    tr.starts_with("<developer_instructions>")
                        && tr.ends_with("</developer_instructions>")
                })
            })
        })
        .count()
}

fn is_auto_mode_classifier_prompt(sys: &str, user_text: &str) -> bool {
    if user_text.starts_with("## Pre-gathered recon (mechanically collected")
        || user_text.contains("Err on the side of blocking.")
        || sys.contains("Err on the side of blocking.")
    {
        return true;
    }
    // Codex Guardian v2 / Code Mode & Node REPL recursive tool-call security classifier
    if (sys.contains("needs blocking security review")
        || user_text.contains("needs blocking security review"))
        && (sys.contains("Review nested calls")
            || user_text.contains("Review nested calls")
            || sys.contains("Predict whether the agent's")
            || user_text.contains("Predict whether the agent's"))
    {
        return true;
    }
    let has_block_tag = user_text.contains("<block>")
        || sys.contains("<block>")
        || user_text.contains("</block>")
        || sys.contains("</block>");
    if has_block_tag
        && (user_text.contains("Stage 1")
            || user_text.contains("Stage 2")
            || sys.contains("Stage 1")
            || sys.contains("Stage 2")
            || sys.contains("auto mode")
            || sys.contains("classifier"))
    {
        return true;
    }
    sys.contains("Transcript classifier") || sys.contains("auto mode classifier")
}

/// Extracts a clean search query hint from a dedicated web-search prompt (or short user message)
/// for populating `server_tool_use` / `web_search_call` metadata when Gemini grounds directly.
#[must_use]
pub fn extract_search_query_hint(messages: &[CanonicalMessage]) -> Option<String> {
    let raw = last_user_plain_text(messages)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    for prefix in [
        "Perform a web search for the query:",
        "Perform a web search for:",
    ] {
        if let Some(after) = trimmed.strip_prefix(prefix) {
            let first_line = after.lines().next().unwrap_or(after).trim();
            if !first_line.is_empty() {
                return Some(first_line.to_owned());
            }
        }
    }
    let single_line = trimmed.lines().next().unwrap_or(trimmed).trim();
    let bounded: String = single_line.chars().take(160).collect();
    (!bounded.is_empty()).then_some(bounded)
}

fn last_user_plain_text(messages: &[CanonicalMessage]) -> Option<String> {
    let msg = messages
        .iter()
        .rev()
        .find(|m| m.role == CanonicalRole::User)?;
    let joined: Vec<&str> = msg
        .parts
        .iter()
        .filter_map(|p| match p {
            CanonicalPart::Text(t) if !t.trim().is_empty() => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join("\n"))
    }
}

/// Extracts unique `[title](https://...)` markdown hyperlinks and their character offsets from `text`.
#[must_use]
pub fn extract_markdown_links(text: &str) -> Vec<MarkdownLinkSpan> {
    let mut results: Vec<MarkdownLinkSpan> = Vec::new();
    let mut rest = text;
    let mut char_cursor: usize = 0;

    while let Some(open_bracket) = rest.find('[') {
        let Some((before, from_bracket)) = rest.split_at_checked(open_bracket) else {
            break;
        };
        char_cursor = char_cursor.saturating_add(before.chars().count());
        let link_start_char = char_cursor;

        let Some((_, after_open_bracket)) = from_bracket.split_at_checked(1) else {
            break;
        };

        // Guard against image syntax `![alt](url)`
        if before.ends_with('!') {
            char_cursor = char_cursor.saturating_add(1);
            rest = after_open_bracket;
            continue;
        }

        if let Some(close_rel) = after_open_bracket.find("](") {
            let Some((raw_title, from_mid)) = after_open_bracket.split_at_checked(close_rel) else {
                break;
            };
            // Reject nested `[` or multi-line `[...]` spans
            if raw_title.contains('[') || raw_title.contains('\n') {
                char_cursor = char_cursor.saturating_add(1);
                rest = after_open_bracket;
                continue;
            }
            let Some((_, after_mid)) = from_mid.split_at_checked(2) else {
                break;
            };
            if let Some(close_paren) = after_mid.find(')') {
                let Some((raw_url_part, from_close_paren)) =
                    after_mid.split_at_checked(close_paren)
                else {
                    break;
                };
                let url = raw_url_part
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim();
                let consumed_chars =
                    1 + raw_title.chars().count() + 2 + raw_url_part.chars().count() + 1;
                let link_end_char = link_start_char.saturating_add(consumed_chars);

                if (url.starts_with("https://") || url.starts_with("http://"))
                    && !url.contains('\n')
                {
                    let clean_title = if raw_title.trim().is_empty() {
                        url.to_owned()
                    } else {
                        raw_title.trim().to_owned()
                    };
                    results.push(MarkdownLinkSpan {
                        title: clean_title,
                        url: url.to_owned(),
                        start_char: link_start_char,
                        end_char: link_end_char,
                    });
                }

                char_cursor = link_end_char;
                let Some((_, after_close)) = from_close_paren.split_at_checked(1) else {
                    break;
                };
                rest = after_close;
                continue;
            }
        }

        char_cursor = char_cursor.saturating_add(1);
        rest = after_open_bracket;
    }

    results
}

/// Parses a `data:<mime>;base64,<payload>` URL into `(media_type, base64_data)`.
pub fn parse_base64_data_url(url: &str) -> Result<(String, String), ValidationError> {
    let rest = url
        .trim()
        .strip_prefix("data:")
        .ok_or(ValidationError::InvalidImageDataUrl)?;
    let (meta, data) = rest
        .split_once(',')
        .ok_or(ValidationError::InvalidImageDataUrl)?;
    let media_type = meta
        .strip_suffix(";base64")
        .ok_or(ValidationError::InvalidImageDataUrl)?
        .trim();
    let cleaned_data = data.trim();
    if media_type.is_empty() || cleaned_data.is_empty() {
        return Err(ValidationError::InvalidImageDataUrl);
    }
    Ok((media_type.to_owned(), cleaned_data.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auto_mode_and_utility_requests_without_false_positives() {
        let user_auto = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(
                "Err on the side of blocking. Stage 1 does NOT apply user intent...\n<block>no</block>"
                    .to_owned(),
            )],
        }];
        assert_eq!(
            detect_auxiliary_task(None, &user_auto, &[], false, false),
            Some(AuxiliaryTaskKind::AutoModeClassifier)
        );

        let web_sub = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(
                "Perform a web search for the query: Singapore weather today".to_owned(),
            )],
        }];
        assert_eq!(
            detect_auxiliary_task(None, &web_sub, &[], true, false),
            Some(AuxiliaryTaskKind::WebSearchSubrequest)
        );

        let recap = vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Refactor src/main.rs".to_owned())],
            },
            CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: vec![CanonicalPart::Text("Working on it.".to_owned())],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(
                    "The user stepped away and is coming back. Recap in under 40 words.".to_owned(),
                )],
            },
        ];
        assert_eq!(
            detect_auxiliary_task(None, &recap, &[], false, false),
            Some(AuxiliaryTaskKind::AwayRecapOrSuggestion)
        );

        // Normal coding chat without tools must NOT be misclassified
        let normal_chat = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(
                "Explain how Stage 1 and Stage 2 compilers bootstrap in Rust.".to_owned(),
            )],
        }];
        assert_eq!(
            detect_auxiliary_task(None, &normal_chat, &[], false, false),
            None
        );

        // OpenCode v2 `PROMPT_TITLE` and `PROMPT_SUMMARY` route to ephemeral fast lanes
        let oc_title_msg = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text(
                "debug 500 errors in production".to_owned(),
            )],
        }];
        assert_eq!(
            detect_auxiliary_task(
                Some(
                    "You name conversations so the user can find them later in a list of past sessions."
                ),
                &oc_title_msg,
                &[],
                false,
                false,
            ),
            Some(AuxiliaryTaskKind::SessionTitleOrTopic)
        );
        assert_eq!(
            detect_auxiliary_task(
                Some(
                    "Summarize what was done in this conversation. Write like a pull request description."
                ),
                &oc_title_msg,
                &[],
                false,
                false,
            ),
            Some(AuxiliaryTaskKind::AwayRecapOrSuggestion)
        );
    }

    #[test]
    fn extracts_markdown_links_and_offsets_accurately() {
        let sample = "Weather via [MSS Portal](https://www.weather.gov.sg/forecast/) and ![img](https://ignore.example/a.png) plus [TimeAndDate](https://www.timeanddate.com/weather/singapore).";
        let links = extract_markdown_links(sample);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].title, "MSS Portal");
        assert_eq!(links[0].url, "https://www.weather.gov.sg/forecast/");
        assert_eq!(links[1].title, "TimeAndDate");
        assert_eq!(
            links[1].url,
            "https://www.timeanddate.com/weather/singapore"
        );
    }

    #[test]
    fn detects_omp_forked_titles_widgets_and_recaps() {
        let dummy_tool = CanonicalToolSpec {
            name: "eval".to_owned(),
            description: Some("Persistent kernel".to_owned()),
            kind: CanonicalToolKind::Function {
                parameters_json: "{}".to_owned(),
            },
        };
        let forked_msgs = vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text("Refactor router".to_owned())],
            },
            CanonicalMessage {
                role: CanonicalRole::Assistant,
                parts: vec![CanonicalPart::Text("Working on it.".to_owned())],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(
                    "<title-request>\nSession UI request, not from the user. Output title JSON.\n</title-request>"
                        .to_owned(),
                )],
            },
        ];
        assert_eq!(
            detect_auxiliary_task(
                Some("You name coding-agent sessions."),
                &forked_msgs,
                &[dummy_tool],
                false,
                false,
            ),
            Some(AuxiliaryTaskKind::SessionTitleOrTopic)
        );

        let widget_msgs = vec![CanonicalMessage {
            role: CanonicalRole::User,
            parts: vec![CanonicalPart::Text("cargo test --workspace".to_owned())],
        }];
        assert_eq!(
            detect_auxiliary_task(
                Some("You design the live status widget of a coding agent's background job."),
                &widget_msgs,
                &[],
                false,
                false,
            ),
            Some(AuxiliaryTaskKind::AwayRecapOrSuggestion)
        );
    }
}
