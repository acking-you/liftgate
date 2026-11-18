use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::acp::{AcpUsage, AvailableModel};
use crate::canonical::{
    CanonicalMessage, CanonicalPart, CanonicalRole, CanonicalToolKind, CanonicalToolSpec,
    NormalizedTurnRequest, ReasoningEffort, detect_auxiliary_task, parse_base64_data_url,
};
use crate::error::ProtocolError;
use crate::session_key::{
    INTERNAL_CODEX_LOCAL_SHELL_TOOL, INTERNAL_CODEX_TOOL_SEARCH_TOOL,
    is_compaction_instructions_prompt, strip_billing_noise, wrap_developer_instructions_block,
};

/// Incoming `POST /v1/responses` request body.
#[derive(Debug, Deserialize)]
pub struct CreateResponseRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input: Option<ResponseInput>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub tools: Vec<ResponseToolSpec>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<ResponseReasoningConfig>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_bounded_metadata")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

const MAX_OPENAI_METADATA_ENTRIES: usize = 64;
const MAX_OPENAI_METADATA_KEY_BYTES: usize = 256;
const MAX_OPENAI_METADATA_VALUE_BYTES: usize = 4096;

fn deserialize_bounded_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedKey(String);

    impl<'de> Deserialize<'de> for BoundedKey {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeyVisitor;
            impl serde::de::Visitor<'_> for KeyVisitor {
                type Value = BoundedKey;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(
                        f,
                        "a metadata key string of at most {MAX_OPENAI_METADATA_KEY_BYTES} bytes"
                    )
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                    if v.len() > MAX_OPENAI_METADATA_KEY_BYTES {
                        return Err(E::custom(format!(
                            "metadata key exceeds {MAX_OPENAI_METADATA_KEY_BYTES} bytes"
                        )));
                    }
                    Ok(BoundedKey(v.to_owned()))
                }
                fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                    if v.len() > MAX_OPENAI_METADATA_KEY_BYTES {
                        return Err(E::custom(format!(
                            "metadata key exceeds {MAX_OPENAI_METADATA_KEY_BYTES} bytes"
                        )));
                    }
                    Ok(BoundedKey(v))
                }
            }
            deserializer.deserialize_string(KeyVisitor)
        }
    }

    struct BoundedScalar(Value);

    impl<'de> Deserialize<'de> for BoundedScalar {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ScalarVisitor;
            impl serde::de::Visitor<'_> for ScalarVisitor {
                type Value = BoundedScalar;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(
                        f,
                        "a scalar metadata value (string <= {MAX_OPENAI_METADATA_VALUE_BYTES} bytes, number, boolean, or null)"
                    )
                }
                fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(BoundedScalar(Value::Bool(v)))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(BoundedScalar(Value::Number(v.into())))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(BoundedScalar(Value::Number(v.into())))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                    let num = serde_json::Number::from_f64(v)
                        .ok_or_else(|| E::custom("non-finite float in metadata"))?;
                    Ok(BoundedScalar(Value::Number(num)))
                }
                fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    Ok(BoundedScalar(Value::Null))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    Ok(BoundedScalar(Value::Null))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                    if v.len() > MAX_OPENAI_METADATA_VALUE_BYTES {
                        return Err(E::custom(format!(
                            "metadata string value exceeds {MAX_OPENAI_METADATA_VALUE_BYTES} bytes"
                        )));
                    }
                    Ok(BoundedScalar(Value::String(v.to_owned())))
                }
                fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                    if v.len() > MAX_OPENAI_METADATA_VALUE_BYTES {
                        return Err(E::custom(format!(
                            "metadata string value exceeds {MAX_OPENAI_METADATA_VALUE_BYTES} bytes"
                        )));
                    }
                    Ok(BoundedScalar(Value::String(v)))
                }
            }
            deserializer.deserialize_any(ScalarVisitor)
        }
    }

    struct MetadataOptionVisitor;

    impl<'de> serde::de::Visitor<'de> for MetadataOptionVisitor {
        type Value = Option<BTreeMap<String, Value>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "null or a metadata object with at most {MAX_OPENAI_METADATA_ENTRIES} scalar entries"
            )
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_map(MetadataMapVisitor)
        }
    }

    struct MetadataMapVisitor;

    impl<'de> serde::de::Visitor<'de> for MetadataMapVisitor {
        type Value = Option<BTreeMap<String, Value>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "a metadata object with at most {MAX_OPENAI_METADATA_ENTRIES} scalar entries"
            )
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some(BoundedKey(k)) = map.next_key::<BoundedKey>()? {
                if out.len() >= MAX_OPENAI_METADATA_ENTRIES && !out.contains_key(&k) {
                    return Err(serde::de::Error::custom(format!(
                        "metadata exceeds maximum of {MAX_OPENAI_METADATA_ENTRIES} entries"
                    )));
                }
                let BoundedScalar(v) = map.next_value::<BoundedScalar>()?;
                out.insert(k, v);
            }
            Ok(Some(out))
        }
    }

    deserializer.deserialize_option(MetadataOptionVisitor)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

/// Flexible input item supporting OpenAI Responses API message items, function calls,
/// custom tool calls (`apply_patch`, Code Mode `exec`), `tool_search_call` / `tool_search_output`,
/// `additional_tools`, `configuration_update`, Codex `local_shell_call` / `apply_patch_call`,
/// tool outputs, and echoed reasoning/compaction items.
#[derive(Debug, Deserialize)]
pub struct ResponseInputItem {
    #[serde(rename = "type", default)]
    pub item_type: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<ResponseMessageContent>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<Value>,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub execution: Option<String>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub reasoning: Option<ResponseReasoningConfig>,
    #[serde(default)]
    pub action: Option<Value>,
    #[serde(default)]
    pub output: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponseMessageContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

#[derive(Debug, Deserialize)]
pub struct ResponseContentPart {
    #[serde(rename = "type", default)]
    pub part_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub image_url: Option<ImageUrlField>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ImageUrlField {
    Direct(String),
    Object { url: String },
}

impl ImageUrlField {
    #[must_use]
    pub fn as_url(&self) -> &str {
        match self {
            Self::Direct(s) => s,
            Self::Object { url } => url,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseToolSpec {
    #[serde(rename = "type")]
    pub tool_type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Box<RawValue>>,
    #[serde(default)]
    pub format: Option<Box<RawValue>>,
    #[serde(default)]
    pub function: Option<NestedFunctionDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponseToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_web_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<WebSearchToolFilters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_domains: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WebSearchToolFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NestedFunctionDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Box<RawValue>>,
}

/// Normalizes an OpenAI `CreateResponseRequest` into a `NormalizedTurnRequest`.
///
/// Important ACP compatibility invariant: OpenAI `/v1/responses` `instructions` and
/// `role: "developer" | "system"` input items (such as Codex CLI's ~25 KB developer prompt)
/// are emitted as `<developer_instructions>` blocks inside `messages` rather than
/// `_meta.antigravity.systemPrompt.append`, avoiding upstream `429 RESOURCE_EXHAUSTED` rejections
/// on large Codex developer prompts while preserving 100% deterministic `SessionPool` prefix
/// hashing across Turn 1..N.
pub fn normalize_responses_request(
    req: &CreateResponseRequest,
) -> Result<NormalizedTurnRequest, ProtocolError> {
    let mut aux_system_parts = Vec::new();
    let mut messages: Vec<CanonicalMessage> = Vec::new();
    let mut has_conversational_input = false;
    let mut deferred_compaction_instructions: Option<String> = None;
    let mut latest_config_effort: Option<ReasoningEffort> = None;
    let mut dynamic_tools: Vec<Value> = Vec::new();

    if let Some(inst) = req.instructions.as_deref() {
        let cleaned = strip_billing_noise(inst);
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            aux_system_parts.push(trimmed.to_owned());
            if req.previous_response_id.is_none() {
                if is_compaction_instructions_prompt(trimmed) {
                    deferred_compaction_instructions = Some(trimmed.to_owned());
                } else if let Some(wrapped) = wrap_developer_instructions_block(trimmed) {
                    messages.push(CanonicalMessage {
                        role: CanonicalRole::User,
                        parts: vec![CanonicalPart::Text(wrapped)],
                    });
                }
            }
        }
    }

    match &req.input {
        Some(ResponseInput::Text(text)) => {
            has_conversational_input = true;
            messages.push(CanonicalMessage {
                role: CanonicalRole::User,
                parts: vec![CanonicalPart::Text(text.clone())],
            });
        }
        Some(ResponseInput::Items(items)) => {
            for item in items {
                let kind = item.item_type.as_deref().unwrap_or("message");
                match kind {
                    "function_call" => {
                        if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                            has_conversational_input = true;
                            let qualified =
                                qualify_namespaced_tool_name(item.namespace.as_deref(), name);
                            push_part(
                                &mut messages,
                                CanonicalRole::Assistant,
                                CanonicalPart::ToolCall {
                                    call_id: call_id.clone(),
                                    name: qualified,
                                    arguments: serialize_input_arguments(item.arguments.as_ref()),
                                    is_custom: false,
                                },
                            );
                        }
                    }
                    "tool_search_call" => {
                        let call_id = item
                            .call_id
                            .clone()
                            .unwrap_or_else(|| INTERNAL_CODEX_TOOL_SEARCH_TOOL.to_owned());
                        has_conversational_input = true;
                        push_part(
                            &mut messages,
                            CanonicalRole::Assistant,
                            CanonicalPart::ToolCall {
                                call_id,
                                name: INTERNAL_CODEX_TOOL_SEARCH_TOOL.to_owned(),
                                arguments: serialize_input_arguments(item.arguments.as_ref()),
                                is_custom: false,
                            },
                        );
                    }
                    "tool_search_output" => {
                        let call_id = item
                            .call_id
                            .clone()
                            .unwrap_or_else(|| INTERNAL_CODEX_TOOL_SEARCH_TOOL.to_owned());
                        has_conversational_input = true;
                        dynamic_tools.extend(item.tools.iter().cloned());
                        let output_json =
                            serde_json::to_string(&item.tools).unwrap_or_else(|_| "[]".to_owned());
                        push_part(
                            &mut messages,
                            CanonicalRole::User,
                            CanonicalPart::ToolResult {
                                call_id,
                                output: output_json,
                                is_error: item.status.as_deref() == Some("failed"),
                            },
                        );
                    }
                    "additional_tools" => {
                        dynamic_tools.extend(item.tools.iter().cloned());
                    }
                    "configuration_update" => {
                        if let Some(eff) = item
                            .reasoning
                            .as_ref()
                            .and_then(|r| r.effort.as_deref())
                            .and_then(ReasoningEffort::from_effort_str)
                        {
                            latest_config_effort = Some(eff);
                        }
                    }
                    "custom_tool_call" => {
                        if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                            has_conversational_input = true;
                            let qualified =
                                qualify_namespaced_tool_name(item.namespace.as_deref(), name);
                            push_part(
                                &mut messages,
                                CanonicalRole::Assistant,
                                CanonicalPart::ToolCall {
                                    call_id: call_id.clone(),
                                    name: qualified,
                                    arguments: item.input.clone().unwrap_or_default(),
                                    is_custom: true,
                                },
                            );
                        }
                    }
                    "local_shell_call" => {
                        if let Some(call_id) = &item.call_id {
                            has_conversational_input = true;
                            let args_json = item
                                .action
                                .as_ref()
                                .map_or_else(|| "{}".to_owned(), normalize_local_shell_action_json);
                            push_part(
                                &mut messages,
                                CanonicalRole::Assistant,
                                CanonicalPart::ToolCall {
                                    call_id: call_id.clone(),
                                    name: INTERNAL_CODEX_LOCAL_SHELL_TOOL.to_owned(),
                                    arguments: args_json,
                                    is_custom: false,
                                },
                            );
                        }
                    }
                    "apply_patch_call" => {
                        if let Some(call_id) = &item.call_id {
                            has_conversational_input = true;
                            push_part(
                                &mut messages,
                                CanonicalRole::Assistant,
                                CanonicalPart::ToolCall {
                                    call_id: call_id.clone(),
                                    name: "apply_patch".to_owned(),
                                    arguments: item.input.clone().unwrap_or_default(),
                                    is_custom: true,
                                },
                            );
                        }
                    }
                    "function_call_output"
                    | "custom_tool_call_output"
                    | "mcp_tool_call_output"
                    | "local_shell_call_output"
                    | "apply_patch_call_output" => {
                        if let Some(call_id) = &item.call_id {
                            has_conversational_input = true;
                            for part in convert_tool_output_parts(call_id, item.output.as_ref())? {
                                push_part(&mut messages, CanonicalRole::User, part);
                            }
                        }
                    }
                    "reasoning" | "web_search_call" | "item_reference" | "compaction"
                    | "compaction_summary" | "compaction_trigger" | "context_compaction" => {}
                    _ => {
                        let role_str = item.role.as_deref().unwrap_or("user");
                        if matches!(role_str, "system" | "developer") {
                            if let Some(content) = &item.content {
                                let extracted = strip_billing_noise(&extract_plain_text(content));
                                let trimmed = extracted.trim();
                                if !trimmed.is_empty() {
                                    if !has_conversational_input {
                                        aux_system_parts.push(trimmed.to_owned());
                                    }
                                    if let Some(wrapped) =
                                        wrap_developer_instructions_block(trimmed)
                                    {
                                        push_part(
                                            &mut messages,
                                            CanonicalRole::User,
                                            CanonicalPart::Text(wrapped),
                                        );
                                    }
                                }
                            }
                            continue;
                        }
                        let role = if role_str == "assistant" {
                            CanonicalRole::Assistant
                        } else {
                            CanonicalRole::User
                        };
                        if let Some(content) = &item.content {
                            let converted = convert_message_content(content)?;
                            if !converted.is_empty() {
                                has_conversational_input = true;
                                messages.push(CanonicalMessage {
                                    role,
                                    parts: converted,
                                });
                            }
                        }
                    }
                }
            }
        }
        None => {}
    }

    if let Some(compaction_inst) = deferred_compaction_instructions
        && let Some(wrapped) = wrap_developer_instructions_block(&compaction_inst)
    {
        has_conversational_input = true;
        push_part(
            &mut messages,
            CanonicalRole::User,
            CanonicalPart::Text(wrapped),
        );
    }

    if !has_conversational_input || messages.is_empty() {
        return Err(ProtocolError::EmptyInput);
    }

    let mut client_tools = Vec::new();
    let mut enable_native_web_search = false;
    let mut enable_native_web_fetch = false;
    let mut web_fetch_max_uses = None;
    let mut web_fetch_allowed_domains = Vec::new();
    let mut web_fetch_blocked_domains = Vec::new();
    let tool_choice_is_none = req
        .tool_choice
        .as_ref()
        .and_then(Value::as_str)
        .is_some_and(|s| s == "none");

    if !tool_choice_is_none {
        for tool in &req.tools {
            match tool.tool_type.as_str() {
                "function" | "custom" => {
                    push_function_or_custom_spec(&mut client_tools, None, tool, None);
                }
                "namespace" => {
                    if let Some(ns_name) = tool
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        && should_activate_tool_namespace(ns_name, &messages)
                    {
                        for sub in &tool.tools {
                            push_function_or_custom_spec(
                                &mut client_tools,
                                Some(ns_name),
                                sub,
                                tool.description.as_deref(),
                            );
                        }
                    }
                }
                "tool_search" => {
                    let params_json = tool.parameters.as_ref().map_or_else(
                        || {
                            "{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"}},\"required\":[\"query\"]}"
                                .to_owned()
                        },
                        |raw| raw.get().to_owned(),
                    );
                    client_tools.push(CanonicalToolSpec {
                        name: INTERNAL_CODEX_TOOL_SEARCH_TOOL.to_owned(),
                        description: tool.description.clone().or_else(|| {
                            Some("Search deferred tools and MCP connectors by query.".to_owned())
                        }),
                        kind: CanonicalToolKind::Function {
                            parameters_json: params_json,
                        },
                    });
                }
                "local_shell" => {
                    client_tools.push(CanonicalToolSpec {
                        name: INTERNAL_CODEX_LOCAL_SHELL_TOOL.to_owned(),
                        description: Some(
                            "Execute a shell command in the Codex workspace. Pass `command` as an argv array of strings and optional `working_directory`."
                                .to_owned(),
                        ),
                        kind: CanonicalToolKind::Function {
                            parameters_json: "{\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"working_directory\":{\"type\":\"string\"},\"timeout_ms\":{\"type\":\"integer\"}},\"required\":[\"command\"]}".to_owned(),
                        },
                    });
                }
                "apply_patch" => {
                    client_tools.push(CanonicalToolSpec {
                        name: tool
                            .name
                            .clone()
                            .unwrap_or_else(|| "apply_patch".to_owned()),
                        description: tool.description.clone().or_else(|| {
                            Some(
                                "Apply a file patch using `*** Begin Patch` ... `*** End Patch` format."
                                    .to_owned(),
                            )
                        }),
                        kind: CanonicalToolKind::Custom {
                            format_json: tool.format.as_ref().map(|r| r.get().to_owned()),
                        },
                    });
                }
                t if t.starts_with("web_search") => {
                    enable_native_web_search = true;
                    // OpenAI Responses API & Codex CLI bundle both web search (`action.type = "search"`)
                    // and URL page opening/fetching (`action.type = "open_page"`) under `{"type": "web_search"}`
                    // whenever `external_web_access` is not explicitly `false`.
                    if tool.external_web_access != Some(false) {
                        enable_native_web_fetch = true;
                    }
                    if tool.max_uses.is_some() {
                        web_fetch_max_uses = tool.max_uses;
                    }
                    web_fetch_allowed_domains.extend(tool.allowed_domains.iter().cloned());
                    if let Some(filters) = &tool.filters {
                        web_fetch_allowed_domains.extend(filters.allowed_domains.iter().cloned());
                    }
                    web_fetch_blocked_domains.extend(tool.blocked_domains.iter().cloned());
                }
                t if t.starts_with("web_fetch") => {
                    enable_native_web_fetch = true;
                    if tool.max_uses.is_some() {
                        web_fetch_max_uses = tool.max_uses;
                    }
                    web_fetch_allowed_domains.extend(tool.allowed_domains.iter().cloned());
                    web_fetch_blocked_domains.extend(tool.blocked_domains.iter().cloned());
                }
                _ => {}
            }
        }

        // Activate any deferred tools discovered via `tool_search_output` or `additional_tools`
        // without duplicating definitions already present in `req.tools`.
        if !dynamic_tools.is_empty() {
            let mut extra_specs = Vec::new();
            for dyn_tool in &dynamic_tools {
                push_dynamic_tool_value(&mut extra_specs, None, dyn_tool, None);
            }
            for spec in extra_specs {
                if !client_tools
                    .iter()
                    .any(|existing| existing.name == spec.name)
                {
                    client_tools.push(spec);
                }
            }
        }
    }

    let aux_system_hint = (!aux_system_parts.is_empty()).then(|| aux_system_parts.join("\n\n"));

    let auxiliary_task = detect_auxiliary_task(
        aux_system_hint.as_deref(),
        &messages,
        &client_tools,
        enable_native_web_search,
        enable_native_web_fetch,
    );

    if let Some(aux) = auxiliary_task {
        client_tools.clear();
        if !matches!(
            aux,
            crate::canonical::AuxiliaryTaskKind::WebSearchSubrequest
        ) {
            enable_native_web_search = false;
            enable_native_web_fetch = false;
        }
    }

    let force_native_web_search = enable_native_web_search
        && req.tool_choice.as_ref().is_some_and(|choice| {
            choice.as_str().is_some_and(|s| {
                s == "required" && client_tools.is_empty() && !enable_native_web_fetch
            }) || choice
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("web_search"))
        });

    let force_native_web_fetch = enable_native_web_fetch
        && req.tool_choice.as_ref().is_some_and(|choice| {
            choice.as_str().is_some_and(|s| {
                s == "required" && client_tools.is_empty() && !enable_native_web_search
            }) || choice
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("web_fetch"))
        });

    let (include_thinking, reasoning_effort) = if auxiliary_task.is_some() {
        (false, Some(ReasoningEffort::Low))
    } else {
        let effort = latest_config_effort.or_else(|| {
            req.reasoning
                .as_ref()
                .and_then(|r| r.effort.as_deref())
                .and_then(ReasoningEffort::from_effort_str)
        });
        let inc = req
            .reasoning
            .as_ref()
            .and_then(|r| r.summary.as_deref())
            .is_none_or(|s| s != "none");
        (inc, effort)
    };

    let include_encrypted_reasoning = req
        .include
        .iter()
        .any(|i| i == "reasoning.encrypted_content");

    Ok(NormalizedTurnRequest {
        requested_model: req.model.clone(),
        system_instructions: None,
        messages,
        client_tools,
        enable_native_web_search,
        force_native_web_search,
        enable_native_web_fetch,
        force_native_web_fetch,
        web_fetch_max_uses,
        web_fetch_allowed_domains,
        web_fetch_blocked_domains,
        auxiliary_task,
        previous_response_id: req.previous_response_id.clone(),
        retain_response_history: req.store.unwrap_or(true),
        reasoning_effort,
        include_thinking,
        omit_thinking_text: false,
        include_encrypted_reasoning,
        stream: req.stream.unwrap_or(false),
    })
}

/// Splits a canonical `"<namespace>.<tool_name>"` identifier back into `(Some(namespace), tool_name)`
/// for OpenAI `/v1/responses` `function_call` output items.
#[must_use]
pub fn split_namespaced_tool_name(full_name: &str) -> (Option<String>, String) {
    if let Some((ns, fn_name)) = full_name.split_once('.')
        && !ns.is_empty()
        && !fn_name.is_empty()
    {
        (Some(ns.to_owned()), fn_name.to_owned())
    } else {
        (None, full_name.to_owned())
    }
}

fn serialize_input_arguments(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => "{}".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_owned()),
    }
}

fn push_dynamic_tool_value(
    out: &mut Vec<CanonicalToolSpec>,
    namespace: Option<&str>,
    val: &Value,
    fallback_desc: Option<&str>,
) {
    let Some(obj) = val.as_object() else {
        return;
    };
    let tool_type = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    match tool_type {
        "function" => {
            if let Some(name) = obj.get("name").and_then(Value::as_str).map(str::trim)
                && !name.is_empty()
            {
                let params_json = obj
                    .get("parameters")
                    .and_then(|p| serde_json::to_string(p).ok())
                    .unwrap_or_else(|| "{}".to_owned());
                let desc = obj
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| fallback_desc.map(str::to_owned));
                out.push(CanonicalToolSpec {
                    name: qualify_namespaced_tool_name(namespace, name),
                    description: desc,
                    kind: CanonicalToolKind::Function {
                        parameters_json: params_json,
                    },
                });
            }
        }
        "custom" => {
            if let Some(name) = obj.get("name").and_then(Value::as_str).map(str::trim)
                && !name.is_empty()
            {
                let format_json = obj
                    .get("format")
                    .and_then(|f| serde_json::to_string(f).ok());
                let desc = obj
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| fallback_desc.map(str::to_owned));
                out.push(CanonicalToolSpec {
                    name: qualify_namespaced_tool_name(namespace, name),
                    description: desc,
                    kind: CanonicalToolKind::Custom { format_json },
                });
            }
        }
        "namespace" => {
            if let Some(ns_name) = obj.get("name").and_then(Value::as_str).map(str::trim)
                && !ns_name.is_empty()
                && let Some(sub_tools) = obj.get("tools").and_then(Value::as_array)
            {
                let ns_desc = obj.get("description").and_then(Value::as_str);
                for sub in sub_tools {
                    push_dynamic_tool_value(out, Some(ns_name), sub, ns_desc);
                }
            }
        }
        _ => {}
    }
}

fn qualify_namespaced_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ns) => format!("{ns}.{name}"),
        None => name.to_owned(),
    }
}

fn push_function_or_custom_spec(
    out: &mut Vec<CanonicalToolSpec>,
    namespace: Option<&str>,
    tool: &ResponseToolSpec,
    fallback_desc: Option<&str>,
) {
    match tool.tool_type.as_str() {
        "function" => {
            if let Some(name) = &tool.name {
                let params_json = tool
                    .parameters
                    .as_ref()
                    .map_or_else(|| "{}".to_owned(), |raw| raw.get().to_owned());
                out.push(CanonicalToolSpec {
                    name: qualify_namespaced_tool_name(namespace, name),
                    description: tool
                        .description
                        .clone()
                        .or_else(|| fallback_desc.map(str::to_owned)),
                    kind: CanonicalToolKind::Function {
                        parameters_json: params_json,
                    },
                });
            } else if let Some(nested) = &tool.function {
                let params_json = nested
                    .parameters
                    .as_ref()
                    .map_or_else(|| "{}".to_owned(), |raw| raw.get().to_owned());
                out.push(CanonicalToolSpec {
                    name: qualify_namespaced_tool_name(namespace, &nested.name),
                    description: nested
                        .description
                        .clone()
                        .or_else(|| fallback_desc.map(str::to_owned)),
                    kind: CanonicalToolKind::Function {
                        parameters_json: params_json,
                    },
                });
            }
        }
        "custom" => {
            if let Some(name) = &tool.name {
                out.push(CanonicalToolSpec {
                    name: qualify_namespaced_tool_name(namespace, name),
                    description: tool
                        .description
                        .clone()
                        .or_else(|| fallback_desc.map(str::to_owned)),
                    kind: CanonicalToolKind::Custom {
                        format_json: tool.format.as_ref().map(|r| r.get().to_owned()),
                    },
                });
            }
        }
        _ => {}
    }
}

/// Core local Codex namespaces (`multi_agent_v1`, `mcp__node_repl`, `mcp__cua_repl`, and custom
/// MCP servers) are always unpacked; deferred remote ChatGPT app connector bundles
/// (`mcp__codex_apps__*`, ~290 KB across 205 tools) are unpacked on-demand when referenced in user
/// messages or prior tool history.
fn should_activate_tool_namespace(ns_name: &str, messages: &[CanonicalMessage]) -> bool {
    let Some(raw_app_slug) = ns_name.strip_prefix("mcp__codex_apps__") else {
        return true;
    };
    let ns_prefix = format!("{ns_name}.");
    for msg in messages {
        for part in &msg.parts {
            if let CanonicalPart::ToolCall { name, .. } = part
                && name.starts_with(&ns_prefix)
            {
                return true;
            }
        }
    }
    let slug_lower = raw_app_slug.trim().to_ascii_lowercase();
    if slug_lower.is_empty() {
        return false;
    }
    let ns_lower = ns_name.to_ascii_lowercase();
    let slug_spaced = slug_lower.replace('_', " ");
    for msg in messages {
        if msg.role != CanonicalRole::User {
            continue;
        }
        for part in &msg.parts {
            if let CanonicalPart::Text(txt) = part {
                let tr = txt.trim();
                if tr.starts_with("<developer_instructions>")
                    && tr.ends_with("</developer_instructions>")
                {
                    continue;
                }
                let lower = tr.to_ascii_lowercase();
                if lower.contains(&ns_lower)
                    || lower.contains(&slug_lower)
                    || lower.contains(&slug_spaced)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn normalize_local_shell_action_json(action: &Value) -> String {
    if let Value::Object(map) = action {
        let mut cleaned = serde_json::Map::new();
        for (k, v) in map {
            if k == "type" || v.is_null() {
                continue;
            }
            if k == "env" && v.as_object().is_some_and(serde_json::Map::is_empty) {
                continue;
            }
            cleaned.insert(k.clone(), v.clone());
        }
        if let Ok(s) = serde_json::to_string(&Value::Object(cleaned)) {
            return s;
        }
    }
    serde_json::to_string(action).unwrap_or_else(|_| "{}".to_owned())
}

fn push_part(messages: &mut Vec<CanonicalMessage>, role: CanonicalRole, part: CanonicalPart) {
    if let Some(last) = messages.last_mut()
        && last.role == role
    {
        last.parts.push(part);
        return;
    }
    messages.push(CanonicalMessage {
        role,
        parts: vec![part],
    });
}

fn extract_plain_text(content: &ResponseMessageContent) -> String {
    match content {
        ResponseMessageContent::Text(s) => s.clone(),
        ResponseMessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn convert_message_content(
    content: &ResponseMessageContent,
) -> Result<Vec<CanonicalPart>, ProtocolError> {
    match content {
        ResponseMessageContent::Text(s) => Ok(vec![CanonicalPart::Text(s.clone())]),
        ResponseMessageContent::Parts(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                match part.part_type.as_str() {
                    "input_image" => {
                        if let Some(img) = &part.image_url {
                            let url = img.as_url();
                            if url.starts_with("data:") {
                                let (media_type, data) = parse_base64_data_url(url)?;
                                out.push(CanonicalPart::ImageBase64 { media_type, data });
                            } else {
                                return Err(ProtocolError::UnsupportedImageUrl);
                            }
                        }
                    }
                    _ => {
                        if let Some(txt) = &part.text {
                            out.push(CanonicalPart::Text(txt.clone()));
                        }
                    }
                }
            }
            Ok(out)
        }
    }
}

fn convert_tool_output_parts(
    call_id: &str,
    output: Option<&Value>,
) -> Result<Vec<CanonicalPart>, ProtocolError> {
    match output {
        None => Ok(vec![CanonicalPart::ToolResult {
            call_id: call_id.to_owned(),
            output: String::new(),
            is_error: false,
        }]),
        Some(Value::String(s)) => Ok(vec![CanonicalPart::ToolResult {
            call_id: call_id.to_owned(),
            output: s.clone(),
            is_error: false,
        }]),
        Some(Value::Array(items)) => {
            let mut text_pieces = Vec::new();
            let mut image_parts = Vec::new();
            let mut saw_structured_part = false;
            for elem in items {
                let Some(obj) = elem.as_object() else {
                    continue;
                };
                let item_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
                match item_type {
                    "input_image" | "image" => {
                        saw_structured_part = true;
                        if let Some(url_val) = obj.get("image_url") {
                            let raw_url = match url_val {
                                Value::String(s) => Some(s.as_str()),
                                Value::Object(m) => m.get("url").and_then(Value::as_str),
                                _ => None,
                            };
                            if let Some(url) = raw_url {
                                if url.starts_with("data:") {
                                    let (media_type, data) = parse_base64_data_url(url)?;
                                    image_parts
                                        .push(CanonicalPart::ImageBase64 { media_type, data });
                                } else {
                                    return Err(ProtocolError::UnsupportedImageUrl);
                                }
                            }
                        } else if let Some(src) = obj.get("source").and_then(Value::as_object) {
                            let src_type =
                                src.get("type").and_then(Value::as_str).unwrap_or_default();
                            let media_type = src
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let data = src.get("data").and_then(Value::as_str).unwrap_or_default();
                            if src_type == "base64" && !media_type.is_empty() && !data.is_empty() {
                                image_parts.push(CanonicalPart::ImageBase64 {
                                    media_type: media_type.to_owned(),
                                    data: data.to_owned(),
                                });
                            } else {
                                return Err(ProtocolError::UnsupportedImageUrl);
                            }
                        }
                    }
                    "input_text" | "output_text" | "text" => {
                        saw_structured_part = true;
                        if let Some(t) = obj.get("text").and_then(Value::as_str) {
                            text_pieces.push(t.to_owned());
                        }
                    }
                    "input_audio" | "audio" => {
                        saw_structured_part = true;
                        text_pieces.push("[Audio attached in tool output]".to_owned());
                    }
                    _ => {
                        if let Some(t) = obj.get("text").and_then(Value::as_str) {
                            saw_structured_part = true;
                            text_pieces.push(t.to_owned());
                        }
                    }
                }
            }
            if !saw_structured_part {
                return Ok(vec![CanonicalPart::ToolResult {
                    call_id: call_id.to_owned(),
                    output: serde_json::to_string(items)?,
                    is_error: false,
                }]);
            }
            let summary_text = if text_pieces.is_empty() && !image_parts.is_empty() {
                "[Image attached in tool output]".to_owned()
            } else {
                text_pieces.join("\n")
            };
            let mut out = Vec::with_capacity(1 + image_parts.len());
            out.push(CanonicalPart::ToolResult {
                call_id: call_id.to_owned(),
                output: summary_text,
                is_error: false,
            });
            out.extend(image_parts);
            Ok(out)
        }
        Some(other) => Ok(vec![CanonicalPart::ToolResult {
            call_id: call_id.to_owned(),
            output: serde_json::to_string(other)?,
            is_error: false,
        }]),
    }
}

// ============================================================================
// Outgoing OpenAI `/v1/responses` Wire Objects & SSE Payloads
// ============================================================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseObject {
    pub id: String,
    pub object: Cow<'static, str>,
    pub created_at: u64,
    pub status: Cow<'static, str>,
    pub error: Option<ResponseInlineError>,
    pub incomplete_details: Option<Value>,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub model: String,
    pub output: Vec<ResponseOutputItem>,
    pub parallel_tool_calls: bool,
    pub previous_response_id: Option<String>,
    pub store: bool,
    pub temperature: f64,
    pub text: ResponseTextFormatConfig,
    pub tool_choice: Value,
    pub tools: Vec<ResponseToolSpec>,
    pub top_p: f64,
    pub truncation: Cow<'static, str>,
    pub usage: Option<ResponsesUsage>,
    pub metadata: BTreeMap<String, Value>,
}

impl ResponseObject {
    /// Measures serialized JSON byte footprint without heap allocation so `SessionPool`
    /// can enforce a global byte budget across cached `GET /v1/responses/{id}` objects.
    #[must_use]
    pub fn estimated_retained_bytes(&self) -> usize {
        struct ByteCounter(usize);

        impl std::io::Write for ByteCounter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(buf.len());
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut counter = ByteCounter(std::mem::size_of::<Self>());
        let _ = serde_json::to_writer(&mut counter, self);
        counter.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseInlineError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseTextFormatConfig {
    pub format: ResponseFormatTag,
}

impl Default for ResponseTextFormatConfig {
    fn default() -> Self {
        Self {
            format: ResponseFormatTag {
                format_type: Cow::Borrowed("text"),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseFormatTag {
    #[serde(rename = "type")]
    pub format_type: Cow<'static, str>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseOutputItem {
    Reasoning {
        id: String,
        summary: Vec<ReasoningSummaryPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    WebSearchCall {
        id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<WebSearchAction>,
    },
    Message {
        id: String,
        status: String,
        role: String,
        content: Vec<OutputMessageContentPart>,
    },
    FunctionCall {
        id: String,
        status: String,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        arguments: String,
    },
    CustomToolCall {
        id: String,
        status: String,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        name: String,
        input: String,
    },
    ToolSearchCall {
        id: String,
        status: String,
        call_id: String,
        execution: String,
        arguments: Value,
    },
    LocalShellCall {
        id: String,
        status: String,
        call_id: String,
        action: LocalShellAction,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalShellAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReasoningSummaryPart {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebSearchAction {
    #[serde(rename = "type")]
    pub action_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<WebSearchSource>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputMessageContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: String,
    pub annotations: Vec<Value>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ResponsesUsage {
    pub input_tokens: u64,
    pub input_tokens_details: InputTokensDetails,
    pub output_tokens: u64,
    pub output_tokens_details: OutputTokensDetails,
    pub total_tokens: u64,
}

impl From<AcpUsage> for ResponsesUsage {
    fn from(u: AcpUsage) -> Self {
        // Antigravity reports cache tokens separately from uncached input, while
        // OpenAI includes them in input_tokens. Its output already includes thinking.
        let input_tokens = u
            .input_tokens
            .saturating_add(u.cached_read_tokens)
            .saturating_add(u.cached_write_tokens);
        Self {
            input_tokens,
            input_tokens_details: InputTokensDetails {
                cached_tokens: u.cached_read_tokens,
            },
            output_tokens: u.output_tokens,
            output_tokens_details: OutputTokensDetails {
                reasoning_tokens: u.thought_tokens.min(u.output_tokens),
            },
            total_tokens: input_tokens.saturating_add(u.output_tokens),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u64,
}

/// OpenAI & Anthropic `/v1/models` list envelope with pagination & capability metadata.
#[derive(Clone, Debug, Serialize)]
pub struct ModelListResponse {
    pub object: &'static str,
    pub data: Vec<ModelCard>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelCard {
    pub id: String,
    pub object: &'static str,
    #[serde(rename = "type")]
    pub item_type: &'static str,
    pub display_name: String,
    pub description: String,
    pub created: u64,
    pub created_at: &'static str,
    pub owned_by: &'static str,
    pub context_window: u64,
    pub auto_compact_token_limit: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

/// OpenAI Codex `model_catalog_json` container (`GET /v1/codex-models`), replacing Codex's
/// built-in model picker catalog with the live Antigravity ACP model catalog and full model
/// metadata so Codex emits zero fallback-metadata warnings.
#[derive(Clone, Debug, Serialize)]
pub struct CodexModelCatalogResponse {
    pub models: Vec<CodexModelCatalogEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexModelCatalogEntry {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_level: &'static str,
    pub supported_reasoning_levels: Vec<CodexReasoningPreset>,
    pub shell_type: &'static str,
    pub visibility: &'static str,
    pub supported_in_api: bool,
    pub priority: i32,
    pub base_instructions: &'static str,
    pub supports_reasoning_summaries: bool,
    pub support_verbosity: bool,
    pub truncation_policy: CodexTruncationPolicy,
    pub supports_parallel_tool_calls: bool,
    pub context_window: i64,
    pub max_context_window: i64,
    pub auto_compact_token_limit: i64,
    pub effective_context_window_percent: i64,
    pub experimental_supported_tools: Vec<String>,
    pub input_modalities: Vec<&'static str>,
    pub supports_image_detail_original: bool,
    pub supports_search_tool: bool,
    pub supports_reasoning_effort_updates: bool,
    pub node_repl_disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_mode: Option<&'static str>,
    pub web_search_tool_type: &'static str,
    pub apply_patch_tool_type: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexReasoningPreset {
    pub effort: &'static str,
    pub description: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CodexTruncationPolicy {
    pub mode: &'static str,
    pub limit: i64,
}

/// Per-tool output byte cap advertised in Codex `model_catalog_json`, scaled up for 500k+ windows
/// while staying well below the single-turn ACP prompt text ceiling (`176,000` bytes).
#[must_use]
pub const fn codex_truncation_limit_bytes(context_window: u64) -> i64 {
    if context_window >= crate::model::GEMINI_DEFAULT_WINDOW {
        80_000
    } else {
        40_000
    }
}

const CODEX_AGY_BASE_INSTRUCTIONS: &str = "\
You are a coding agent running in the Codex CLI, a terminal-based coding assistant. Be precise, safe, and helpful.

# Working Principles
- Keep going autonomously until the user's request is completely resolved before yielding back. Do not guess when you can inspect the workspace directly.
- Obey any `AGENTS.md` instructions whose directory scope covers files you touch; deeper `AGENTS.md` files take precedence over parent ones, and direct user/developer instructions take precedence over `AGENTS.md`.
- Before grouped tool calls or long edits, send a brief 1-sentence preamble so the user can follow progress.
- When searching text or file paths in the shell, prefer `rg` and `rg --files`.
- Use `apply_patch` for file edits (`*** Begin Patch` ... `*** End Patch`), keep changes surgical and consistent with existing codebase style, and never commit or create git branches unless asked.
- When Code Mode (`exec` and `wait`) is active, prefer orchestrating multi-step or parallel read/search/inspection calls inside a single `exec` JavaScript cell (`await Promise.all([tools.exec_command(...), ...])`), store reusable intermediate state with `store(key, value)` / `load(key)`, return concise summaries via `text(...)`, and call `wait` if `exec` yields a running cell ID.
- Validate changes with targeted builds/tests when appropriate in non-interactive or test-focused runs; do not fix unrelated issues outside the user's scope.
- When `request_user_input_async` or `request_user_input` is available and a critical requirement cannot be derived from workspace inspection, ask concise questions while continuing independent work.
- For live web searches or reading HTTP/HTTPS URLs, prefer your built-in `search_web` and server-managed `__gateway_web_fetch` tools rather than running `curl` or `Invoke-WebRequest` in local shell commands (which may be blocked by Codex's network sandbox).

# Response Style
- Respond concisely and naturally like a direct teammate. Skip heavy section headers for simple greetings, confirmations, or short answers.
- Wrap commands, paths, env vars, and identifiers in backticks (`...`).
- Format file references as standalone inline code paths such as `src/lib.rs:42` (never emit `file://` URIs or `【F:...】` citations in Codex CLI output).";

/// Builds a complete Codex `model_catalog_json` document from the live ACP `AvailableModel` list,
/// ordering `default_model` first (`priority = 0`) followed by all remaining Antigravity models.
#[must_use]
pub fn build_codex_model_catalog(
    available: &[AvailableModel],
    default_model: &str,
    long_context: bool,
) -> CodexModelCatalogResponse {
    build_codex_model_catalog_with_tool_mode(available, default_model, long_context, None)
}

/// Builds a Codex `model_catalog_json` document with an optional explicit `tool_mode`
/// (`"code_mode"`, `"code_mode_only"`, or `"direct"`). Defaults to `"code_mode"` when `tool_mode`
/// is `None`, enabling Codex's V8 JavaScript programmatic tool orchestration (`exec` + `wait`)
/// alongside direct tools out of the box.
#[must_use]
pub fn build_codex_model_catalog_with_tool_mode(
    available: &[AvailableModel],
    default_model: &str,
    long_context: bool,
    tool_mode: Option<&str>,
) -> CodexModelCatalogResponse {
    let resolved_tool_mode = match tool_mode.map(str::trim) {
        None | Some("") => Some("code_mode"),
        Some(m) if m.eq_ignore_ascii_case("code_mode") => Some("code_mode"),
        Some(m) if m.eq_ignore_ascii_case("code_mode_only") => Some("code_mode_only"),
        Some(m)
            if m.eq_ignore_ascii_case("direct")
                || m.eq_ignore_ascii_case("off")
                || m.eq_ignore_ascii_case("none") =>
        {
            Some("direct")
        }
        Some(_) => Some("code_mode"),
    };
    let mut ordered: Vec<(u8, usize, &str, String)> = available
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let id = m.model_id.trim();
            let display = if m.name.trim().is_empty() {
                id.to_owned()
            } else {
                m.name.trim().to_owned()
            };
            let tier = codex_catalog_sort_tier(id, default_model);
            (tier, idx, id, display)
        })
        .collect();

    if ordered.is_empty() && !default_model.trim().is_empty() {
        let def = default_model.trim();
        ordered.push((0, 0, def, def.to_owned()));
    }

    ordered.sort_by_key(|(tier, idx, _, _)| (*tier, *idx));

    let presets = vec![
        CodexReasoningPreset {
            effort: "low",
            description: "Fast low-latency reasoning",
        },
        CodexReasoningPreset {
            effort: "medium",
            description: "Balanced speed and reasoning depth",
        },
        CodexReasoningPreset {
            effort: "high",
            description: "Deep multi-step coding and analysis",
        },
        CodexReasoningPreset {
            effort: "xhigh",
            description: "Maximum extended reasoning budget",
        },
    ];

    let models = ordered
        .into_iter()
        .enumerate()
        .map(|(rank, (_, _, slug, display_name))| {
            let lower = slug.to_ascii_lowercase();
            let (window, compact) = crate::model::context_limits(slug, long_context);
            let context_window = i64::try_from(window).unwrap_or(i64::MAX);
            let auto_compact = i64::try_from(compact).unwrap_or(i64::MAX);
            let default_effort = if lower.ends_with("-xhigh") {
                "xhigh"
            } else if lower.ends_with("-medium") {
                "medium"
            } else if lower.ends_with("-low") {
                "low"
            } else {
                "high"
            };
            let ctx_k = context_window / 1_000;
            let description = format!("{ctx_k}k ctx · Antigravity ACP ({slug})");
            let priority = i32::try_from(rank).unwrap_or(i32::MAX);

            CodexModelCatalogEntry {
                slug: slug.to_owned(),
                display_name,
                description,
                default_reasoning_level: default_effort,
                supported_reasoning_levels: presets.clone(),
                shell_type: "shell_command",
                visibility: "list",
                supported_in_api: true,
                priority,
                base_instructions: CODEX_AGY_BASE_INSTRUCTIONS,
                supports_reasoning_summaries: true,
                support_verbosity: false,
                truncation_policy: CodexTruncationPolicy {
                    mode: "bytes",
                    limit: codex_truncation_limit_bytes(window),
                },
                supports_parallel_tool_calls: true,
                context_window,
                max_context_window: context_window,
                auto_compact_token_limit: auto_compact,
                effective_context_window_percent: 95,
                experimental_supported_tools: vec![
                    "request_user_input_async".to_owned(),
                    "send_user_message_async".to_owned(),
                    "send_message_to_user_async".to_owned(),
                    "clock".to_owned(),
                ],
                input_modalities: vec!["text", "image"],
                supports_image_detail_original: true,
                supports_search_tool: false,
                supports_reasoning_effort_updates: true,
                node_repl_disabled: false,
                tool_mode: resolved_tool_mode,
                web_search_tool_type: "text_and_image",
                apply_patch_tool_type: "freeform",
            }
        })
        .collect();

    CodexModelCatalogResponse { models }
}

fn codex_catalog_sort_tier(model_id: &str, default_model: &str) -> u8 {
    if model_id.eq_ignore_ascii_case(default_model) {
        return 0;
    }
    let lower = model_id.to_ascii_lowercase();
    if lower.starts_with("gemini-4-") && (lower.ends_with("-high") || lower.ends_with("-xhigh")) {
        1
    } else if lower.starts_with("claude-") && lower.ends_with("-high") {
        2
    } else if lower.contains("orchestrator") || lower.ends_with("-medium") {
        3
    } else if lower.contains("flash") {
        4
    } else {
        5
    }
}

/// Standard OpenAI error response envelope.
#[derive(Clone, Debug, Serialize)]
pub struct OpenAiErrorEnvelope {
    pub error: OpenAiErrorBody,
}

#[derive(Clone, Debug, Serialize)]
pub struct OpenAiErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: &'static str,
    pub code: &'static str,
    pub param: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mock_available_models;
    use serde_json::json;

    #[test]
    fn extracts_multimodal_images_from_function_call_output_and_advertises_async_user_tools() {
        let req: CreateResponseRequest = serde_json::from_value(json!({
            "model": "gemini-4-carbon-high",
            "input": [
                { "role": "user", "content": "Inspect screenshot.png" },
                {
                    "type": "function_call",
                    "call_id": "call_img_1",
                    "name": "view_image",
                    "arguments": "{\"path\":\"screenshot.png\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_img_1",
                    "output": [
                        { "type": "input_text", "text": "Loaded screenshot.png (800x600)" },
                        { "type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==" }
                    ]
                }
            ]
        }))
        .expect("valid CreateResponseRequest");

        let norm = normalize_responses_request(&req).expect("normalized turn");
        let last_msg = norm.messages.last().expect("tool output message");
        assert_eq!(last_msg.parts.len(), 2);
        assert!(matches!(
            &last_msg.parts[0],
            CanonicalPart::ToolResult {
                call_id,
                output,
                is_error: false
            } if call_id == "call_img_1" && output.contains("Loaded screenshot.png") && !output.contains("iVBORw0KGgo")
        ));
        assert!(matches!(
            &last_msg.parts[1],
            CanonicalPart::ImageBase64 { media_type, data }
                if media_type == "image/png" && data == "iVBORw0KGgoAAAANSUhEUg=="
        ));

        let catalog =
            build_codex_model_catalog(&mock_available_models(), "gemini-4-carbon-high", false);
        let first = catalog.models.first().expect("non-empty catalog");
        assert_eq!(
            (first.context_window, first.auto_compact_token_limit),
            (500_000, 425_000)
        );
        assert_eq!(first.truncation_policy.limit, 80_000);
        let experimental =
            build_codex_model_catalog(&mock_available_models(), "gemini-4-carbon-high", true);
        let first_long = experimental.models.first().unwrap();
        assert_eq!(
            (
                first_long.context_window,
                first_long.auto_compact_token_limit
            ),
            (1_000_000, 850_000)
        );
        for tool in [
            "request_user_input_async",
            "send_user_message_async",
            "send_message_to_user_async",
            "clock",
        ] {
            assert!(
                first.experimental_supported_tools.iter().any(|t| t == tool),
                "missing experimental tool {tool}"
            );
        }
        assert!(first.supports_image_detail_original);
        assert!(first.supports_reasoning_effort_updates);
        assert!(!first.supports_search_tool);
        assert!(!first.node_repl_disabled);
        assert_eq!(first.tool_mode, Some("code_mode"));
        assert_eq!(first.web_search_tool_type, "text_and_image");

        let direct_cat = build_codex_model_catalog_with_tool_mode(
            &mock_available_models(),
            "gemini-4-carbon-high",
            false,
            Some("direct"),
        );
        assert_eq!(direct_cat.models[0].tool_mode, Some("direct"));

        let cmo_cat = build_codex_model_catalog_with_tool_mode(
            &mock_available_models(),
            "gemini-4-carbon-high",
            false,
            Some("code_mode_only"),
        );
        assert_eq!(cmo_cat.models[0].tool_mode, Some("code_mode_only"));
    }

    #[test]
    fn normalizes_codex_code_mode_tool_search_and_configuration_update_items() {
        let raw_payload = json!({
            "model": "gemini-4-carbon-high",
            "reasoning": { "effort": "medium" },
            "tools": [
                {
                    "type": "custom",
                    "name": "exec",
                    "description": "Run JavaScript code.\ndeclare const tools: { exec_command(args: { cmd: string }): Promise<{ output: string }>; };",
                    "format": { "type": "grammar", "syntax": "lark", "definition": "start: /[\\s\\S]+/" }
                },
                {
                    "type": "function",
                    "name": "wait",
                    "description": "Wait on a running cell.",
                    "parameters": { "type": "object", "properties": { "cell_id": { "type": "string" } }, "required": ["cell_id"] }
                },
                {
                    "type": "tool_search",
                    "execution": "client",
                    "description": "Search deferred tools.",
                    "parameters": { "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] }
                }
            ],
            "input": [
                { "role": "user", "content": "Search calendar tools and inspect repo" },
                {
                    "type": "tool_search_call",
                    "call_id": "search-1",
                    "execution": "client",
                    "arguments": { "query": "calendar", "limit": 2 }
                },
                {
                    "type": "tool_search_output",
                    "call_id": "search-1",
                    "status": "completed",
                    "execution": "client",
                    "tools": [{
                        "type": "function",
                        "name": "mcp__calendar__list_events",
                        "description": "List events.",
                        "defer_loading": true,
                        "parameters": { "type": "object" }
                    }]
                },
                {
                    "type": "custom_tool_call",
                    "call_id": "call_exec_1",
                    "name": "exec",
                    "input": "const r = await tools.exec_command({ cmd: \"cargo --version\" });\ntext(r.output);"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_exec_1",
                    "output": [
                        { "type": "input_text", "text": "Script completed\ncargo 1.86.0" },
                        { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "wav" } }
                    ]
                },
                {
                    "type": "configuration_update",
                    "reasoning": { "effort": "ultra" }
                }
            ]
        });
        let req: CreateResponseRequest = serde_json::from_slice(
            &serde_json::to_vec(&raw_payload).expect("serializable payload"),
        )
        .expect("valid Code Mode + tool_search + configuration_update request");

        let norm = normalize_responses_request(&req).expect("normalized Code Mode turn");
        assert_eq!(norm.reasoning_effort, Some(ReasoningEffort::XHigh));
        let tool_names: Vec<&str> = norm.client_tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            tool_names,
            vec!["exec", "wait", "tool_search", "mcp__calendar__list_events"]
        );
        let last_msg = norm.messages.last().expect("custom tool output message");
        assert!(matches!(
            last_msg.parts.last(),
            Some(CanonicalPart::ToolResult { call_id, output, is_error: false })
                if call_id == "call_exec_1"
                    && output.contains("cargo 1.86.0")
                    && output.contains("[Audio attached in tool output]")
        ));
    }
}
