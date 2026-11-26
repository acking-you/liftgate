use liftgate_harness::fs_util::remove_key_preserving_order;
use liftgate_harness::ledger::ledger_key_for_path;
use liftgate_harness::opencode::read_existing_opencode_json_object;
use serde_json::json;

use super::*;
use crate::state::write_file_atomic;

#[test]
fn claude_settings_merge_and_uninstall_preserve_unrelated_user_keys() {
    let mut existing: Map<String, Value> = serde_json::from_value(json!({
        "alwaysThinkingEnabled": true,
        "modelSettings": {
            "gemini-4-carbon-high": { "effortLevel": "high" },
            "unrelated-model": { "autoCompactWindow": 450000, "effortLevel": "low" }
        },
        "env": {
            "CUSTOM_USER_VAR": "keep-me",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000"
        }
    }))
    .unwrap();

    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: false,
        no_code_mode: false,
        discovered_models: vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: "Gemini 4 Carbon (High)".to_owned(),
            context_window: 500_000,
            auto_compact_token_limit: 425_000,
        }],
    };

    merged_claude_settings_into(&mut existing, &params);
    assert_eq!(existing["alwaysThinkingEnabled"], true);
    assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
    assert_eq!(
        existing["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:8787"
    );
    assert_eq!(existing["env"]["CLAUDE_CODE_EXPERIMENTAL_CODE_MODE"], "1");
    assert_eq!(existing["model"], "gemini-4-carbon-high");
    assert_eq!(existing["skipDangerousModePermissionPrompt"], true);
    assert!(
        existing["env"]
            .get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
            .is_none()
    );
    assert!(
        existing["env"]
            .get("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
            .is_none()
    );
    assert_eq!(
        existing["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
        425000
    );
    let merged_once = existing.clone();
    merged_claude_settings_into(&mut existing, &params);
    assert_eq!(existing, merged_once);

    let mut standard_perms = params.clone();
    standard_perms.no_bypass = true;
    standard_perms.no_code_mode = true;
    merged_claude_settings_into(&mut existing, &standard_perms);
    assert!(existing.get("skipDangerousModePermissionPrompt").is_none());
    assert!(existing.get("permissions").is_none());
    assert_eq!(existing["env"]["CLAUDE_CODE_EXPERIMENTAL_CODE_MODE"], "0");

    strip_claude_settings(&mut existing);
    assert_eq!(existing["alwaysThinkingEnabled"], true);
    assert_eq!(existing["env"]["CUSTOM_USER_VAR"], "keep-me");
    assert!(existing["env"].get("ANTHROPIC_BASE_URL").is_none());
    assert!(
        existing["env"]
            .get("CLAUDE_CODE_EXPERIMENTAL_CODE_MODE")
            .is_none()
    );
    assert!(existing.get("modelOverrides").is_none());
    assert_eq!(
        existing["modelSettings"]["gemini-4-carbon-high"]["effortLevel"],
        "high"
    );
    assert!(
        existing["modelSettings"]["gemini-4-carbon-high"]
            .get("autoCompactWindow")
            .is_none()
    );
    assert_eq!(
        existing["modelSettings"]["unrelated-model"]["autoCompactWindow"],
        450000
    );
    assert!(existing.get("autoCompactWindow").is_none());
}

#[test]
fn claude_global_auto_compact_window_is_only_managed_when_liftgate_wrote_it() {
    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: Vec::new(),
    };

    // A user-chosen global window survives both merge and uninstall.
    let mut custom: Map<String, Value> =
        serde_json::from_value(json!({ "autoCompactWindow": 300000 })).unwrap();
    merged_claude_settings_into(&mut custom, &params);
    assert_eq!(custom["autoCompactWindow"], 300000);
    strip_claude_settings(&mut custom);
    assert_eq!(custom["autoCompactWindow"], 300000);

    // Without a Liftgate picker, a value equal to our fallback is still the user's.
    let mut unmanaged: Map<String, Value> =
        serde_json::from_value(json!({ "autoCompactWindow": 110000 })).unwrap();
    strip_claude_settings(&mut unmanaged);
    assert_eq!(unmanaged["autoCompactWindow"], 110000);

    // Our own fallback is written once and removed cleanly.
    let mut managed = Map::new();
    merged_claude_settings_into(&mut managed, &params);
    assert_eq!(managed["autoCompactWindow"], 110000);
    let merged_once = managed.clone();
    merged_claude_settings_into(&mut managed, &params);
    assert_eq!(managed, merged_once);
    strip_claude_settings(&mut managed);
    assert!(managed.get("autoCompactWindow").is_none());
}

#[test]
fn context_policy_tracks_live_models_and_limits_shared_claude_aliases() {
    let models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-carbon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gemini-3.8-flash-low", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gpt-oss-120b", "context_window": 128000, "auto_compact_token_limit": 110000},
            {"id": "claude-opus-5-5", "context_window": 200000, "auto_compact_token_limit": 170000}
        ])).unwrap();
    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: models,
    };
    let settings = build_claude_settings_map(&params);
    assert_eq!(settings["autoCompactWindow"], 110000);
    assert_eq!(
        settings["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
        425000
    );
    // The shared canonical ID must honor the smaller real Claude deployment.
    assert_eq!(
        settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
        170000
    );
    assert_eq!(
        settings["modelSettings"]["claude-haiku-4-5-20251001"]["autoCompactWindow"],
        110000
    );
    assert_eq!(
        settings["modelSettings"]["gpt-oss-120b"]["autoCompactWindow"],
        110000
    );
    let catalog: Value = serde_json::from_slice(
        &build_offline_codex_catalog_bytes_with_mode(
            &params.discovered_models,
            &params.primary_model,
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let catalog_models = catalog["models"].as_array().unwrap();
    let gemini = catalog_models
        .iter()
        .find(|m| m["slug"] == params.primary_model)
        .unwrap();
    assert_eq!(gemini["context_window"], 500000);
    assert_eq!(gemini["auto_compact_token_limit"], 425000);
    assert_eq!(gemini["truncation_policy"]["limit"], 80000);
    assert_eq!(gemini["tool_mode"], "code_mode");
    let gpt_oss = catalog_models
        .iter()
        .find(|m| m["slug"] == "gpt-oss-120b")
        .unwrap();
    assert_eq!(gpt_oss["truncation_policy"]["limit"], 40000);
    let legacy: DiscoveredModel = serde_json::from_value(json!({
        "id": "gemini-4-carbon-high", "context_window": 1000000
    }))
    .unwrap();
    assert_eq!(legacy.context_limits(), (500000, 425000));

    // Standard 500k live catalog containing both Gemini 4 Carbon (500k / 425k) and real
    // `claude-sonnet-4-6` (200k / 170k) keeps 425k on Gemini + `claude-opus-5-5` without
    // hijacking `claude-sonnet-4-6` in `modelOverrides`.
    let live_500k_models: Vec<DiscoveredModel> = serde_json::from_value(json!([
            {"id": "gemini-4-carbon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "gemini-4-hydrogen-low", "context_window": 500000, "auto_compact_token_limit": 425000},
            {"id": "claude-sonnet-4-6", "context_window": 200000, "auto_compact_token_limit": 170000}
        ]))
        .unwrap();
    let live_500k_params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-4-hydrogen-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: live_500k_models,
    };
    let live_500k_settings = build_claude_settings_map(&live_500k_params);
    assert_eq!(
        live_500k_settings["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
        425000
    );
    assert_eq!(
        live_500k_settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
        425000
    );
    assert_eq!(
        live_500k_settings["modelSettings"]["claude-sonnet-4-6"]["autoCompactWindow"],
        170000
    );
    assert!(
        live_500k_settings["modelOverrides"]
            .get("claude-sonnet-4-6")
            .is_none()
    );
    assert_eq!(
        live_500k_settings["modelOverrides"]["claude-opus-5-5"],
        "gemini-4-carbon-high"
    );

    // Re-syncing over an older 280k/245k settings map cleans up retired model aliases while
    // preserving unrelated user-configured `modelSettings` entries.
    let mut stale_v020: Map<String, Value> = serde_json::from_value(json!({
        "modelOverrides": {
            "claude-opus-5-5": "gemini-4-argon",
            "claude-sonnet-4-6": "gemini-4-argon"
        },
        "modelPicker": {
            "replaceBuiltInOptions": true,
            "options": [{
                "model": "gemini-4-argon-high",
                "label": "Gemini 4 Argon (High)",
                "description": "280k ctx · Antigravity ACP (gemini-4-argon-high)",
                "behavesAs": "claude-opus-5-5"
            }]
        },
        "modelSettings": {
            "gemini-4-argon": { "autoCompactWindow": 245000 },
            "gemini-4-argon-high": { "autoCompactWindow": 245000 },
            "claude-sonnet-4-6": { "autoCompactWindow": 245000 },
            "unrelated-model": { "autoCompactWindow": 450000, "effortLevel": "low" }
        }
    }))
    .unwrap();
    merged_claude_settings_into(&mut stale_v020, &live_500k_params);
    assert!(stale_v020["modelSettings"].get("gemini-4-argon").is_none());
    assert!(
        stale_v020["modelSettings"]
            .get("gemini-4-argon-high")
            .is_none()
    );
    assert_eq!(
        stale_v020["modelSettings"]["claude-sonnet-4-6"]["autoCompactWindow"],
        170000
    );
    assert_eq!(
        stale_v020["modelSettings"]["gemini-4-carbon-high"]["autoCompactWindow"],
        425000
    );
    assert_eq!(
        stale_v020["modelSettings"]["unrelated-model"]["autoCompactWindow"],
        450000
    );

    // Alias primary models (`gemini-4-neon`) resolve against the live catalog entries.
    let alias_models: Vec<DiscoveredModel> = serde_json::from_value(json!([
        {"id": "gemini-4-neon-high", "context_window": 500000, "auto_compact_token_limit": 425000},
        {"id": "gemini-3.8-flash-low", "context_window": 500000, "auto_compact_token_limit": 425000}
    ]))
    .unwrap();
    let alias_params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-neon".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: alias_models,
    };
    let alias_settings = build_claude_settings_map(&alias_params);
    assert_eq!(
        alias_settings["modelSettings"]["gemini-4-neon"]["autoCompactWindow"],
        425000
    );
    assert_eq!(
        alias_settings["modelSettings"]["claude-opus-5-5"]["autoCompactWindow"],
        425000
    );
}

#[test]
fn codex_toml_merge_is_idempotent_keeps_root_before_tables_and_uninstalls_cleanly() {
    let original = "# User header\ncustom_flag = true\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 850000\n\n[projects.\"D:\\\\repo\"]\ntrust_level = \"trusted\"\n\n[features]\nfast_mode = true\n\n[mcp_servers.demo]\ncommand = \"npx\"\n";
    let catalog = Path::new(r"C:\Users\alice\.codex\agy-models.json");

    let merged_once = merge_codex_config_toml(
        original,
        "http://127.0.0.1:8787",
        "gemini-4-carbon-high",
        catalog,
        true,
        false,
        false,
    );
    let merged_twice = merge_codex_config_toml(
        &merged_once,
        "http://127.0.0.1:8787",
        "gemini-4-carbon-high",
        catalog,
        true,
        false,
        false,
    );
    assert_eq!(merged_once, merged_twice);

    // Root keys MUST precede the first `[` table header.
    let first_table_pos = merged_twice.find("\n[").unwrap();
    let model_pos = merged_twice
        .find("model = \"gemini-4-carbon-high\"")
        .unwrap();
    assert!(model_pos < first_table_pos);
    assert!(merged_twice.contains("suppress_unstable_features_warning = true"));
    assert!(merged_twice.contains("code_mode = true"));
    assert!(merged_twice.contains("fast_mode = true"));
    assert!(merged_twice.contains("C:/Users/alice/.codex/agy-models.json"));
    assert!(merged_twice.contains("[mcp_servers.demo]"));
    assert!(merged_twice.contains("custom_flag = true"));
    assert!(!merged_twice.contains("model_context_window"));
    assert!(!merged_twice.contains("model_auto_compact_token_limit"));

    let stripped = strip_codex_config_toml(&merged_twice);
    assert!(!stripped.contains("[model_providers.agy]"));
    assert!(!stripped.contains("model_provider"));
    assert!(!stripped.contains("suppress_unstable_features_warning"));
    assert!(!stripped.contains("code_mode"));
    assert!(stripped.contains("fast_mode = true"));
    assert!(stripped.contains("custom_flag = true"));
    assert!(stripped.contains("[mcp_servers.demo]"));
}

#[test]
fn relative_project_dir_resolves_to_absolute_codex_catalog_path() {
    let (_, catalog) = resolve_codex_paths(false, Some(Path::new("."))).unwrap();
    assert!(catalog.is_absolute());
}

#[test]
fn isolated_codex_app_toml_forces_file_apikey_auth_and_strips_conflicting_user_auth_keys() {
    let user_toml = "\
# User settings
custom_flag = true
cli_auth_credentials_store = \"keyring\"
forced_login_method = \"chatgpt\"
forced_chatgpt_workspace_id = \"ws-123\"
check_for_update_on_startup = true
openai_base_url = \"https://example.invalid/v1\"
profile = \"work\"

[features]
remote_control = true
in_app_updates = true
remote_plugin = true
recommended_plugins = true
fast_mode = true

[marketplaces.openai-bundled]
source = \"C:\\\\Users\\\\alice\\\\.codex\\\\.tmp\\\\bundled-marketplaces\\\\openai-bundled\"

[marketplaces.openai-primary-runtime]
source = \"C:\\\\Users\\\\alice\\\\.cache\\\\codex-runtimes\\\\codex-primary-runtime\"

[model_providers.agy.auth]
command = \"echo bad\"

[mcp_servers.demo]
command = \"npx\"
";
    let catalog = Path::new(r"C:\Users\alice\.liftgate\desktop\sessions\s1\agy-models.json");
    let rendered = merge_isolated_codex_app_config_toml(
        user_toml,
        "http://127.0.0.1:8787",
        "gemini-4-carbon-high",
        catalog,
        "local-agy",
        false,
        false,
        false,
    );
    let rendered_again = merge_isolated_codex_app_config_toml(
        &rendered,
        "http://127.0.0.1:8787",
        "gemini-4-carbon-high",
        catalog,
        "local-agy",
        false,
        false,
        false,
    );
    assert_eq!(rendered, rendered_again);
    assert!(rendered.contains("cli_auth_credentials_store = \"file\""));
    assert!(rendered.contains("forced_login_method = \"api\""));
    assert!(rendered.contains("check_for_update_on_startup = false"));
    assert!(rendered.contains("requires_openai_auth = false"));
    assert!(rendered.contains("experimental_bearer_token = \"local-agy\""));
    assert!(rendered.contains("remote_control = false"));
    assert!(rendered.contains("in_app_updates = false"));
    assert!(rendered.contains("remote_plugin = false"));
    assert!(rendered.contains("recommended_plugins = false"));
    assert!(rendered.contains("fast_mode = true"));
    assert!(rendered.contains("[marketplaces.openai-primary-runtime]"));
    assert!(rendered.contains("[mcp_servers.demo]"));
    assert!(!rendered.contains("keyring"));
    assert!(!rendered.contains("forced_chatgpt_workspace_id"));
    assert!(!rendered.contains("check_for_update_on_startup = true"));
    assert!(!rendered.contains("openai_base_url"));
    assert!(!rendered.contains("profile ="));
    assert!(!rendered.contains("[marketplaces.openai-bundled]"));
    assert!(!rendered.contains("[model_providers.agy.auth]"));
    assert!(!rendered.contains("remote_control = true"));
    assert!(!rendered.contains("in_app_updates = true"));
    assert!(!rendered.contains("remote_plugin = true"));
    assert!(!rendered.contains("recommended_plugins = true"));
}

#[test]
fn opencode_config_merge_and_uninstall_preserve_unrelated_user_fields() {
    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "agy/gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-4-hydrogen-low".to_owned(),
        no_bypass: false,
        no_code_mode: true,
        discovered_models: vec![DiscoveredModel {
            id: "gemini-4-carbon-high".to_owned(),
            display_name: "Gemini 4 Carbon (High)".to_owned(),
            context_window: 500_000,
            auto_compact_token_limit: 425_000,
        }],
    };

    let mut existing: Map<String, Value> = serde_json::from_value(json!({
        "theme": "tokyonight",
        "mcp": {
            "playwright": { "type": "local", "command": ["npx", "@playwright/mcp"] }
        },
        "providers": {
            "agy": { "name": "Stale v2 entry" },
            "openrouter": { "name": "OpenRouter" }
        },
        "permission": {
            "doom_loop": "ask"
        }
    }))
    .expect("valid json object");

    merged_opencode_config_into(&mut existing, &params, true);
    assert_eq!(
        existing.get("model").and_then(Value::as_str),
        Some("agy/gemini-4-carbon-high")
    );
    assert_eq!(
        existing.get("small_model").and_then(Value::as_str),
        Some("agy/gemini-4-hydrogen-low")
    );
    assert_eq!(
        existing.get("theme").and_then(Value::as_str),
        Some("tokyonight")
    );
    assert!(existing.get("mcp").is_some());
    // Stale `providers.agy` removed while `providers.openrouter` preserved
    assert!(existing["providers"].get("agy").is_none());
    assert!(existing["providers"].get("openrouter").is_some());
    assert_eq!(
        existing["provider"]["agy"]["npm"].as_str(),
        Some("@ai-sdk/anthropic")
    );
    assert_eq!(
        existing["provider"]["agy"]["options"]["baseURL"].as_str(),
        Some("http://127.0.0.1:8787/v1")
    );
    assert_eq!(
        existing["provider"]["agy"]["models"]["gemini-4-carbon-high"]["limit"]["context"].as_u64(),
        Some(500_000)
    );
    assert_eq!(
        existing["provider"]["agy"]["models"]["gemini-4-carbon-high"]["modalities"]["input"],
        json!(["text", "image"])
    );
    // Last-match-wins permission ordering: `*` -> `doom_loop` -> `execute: deny`
    let perm_keys: Vec<&str> = existing["permission"]
        .as_object()
        .expect("permission object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(perm_keys, vec!["*", "doom_loop", "execute"]);
    assert_eq!(existing["compaction"]["background"].as_bool(), Some(false));
    assert_eq!(
        existing["experimental"]["full_codemode"].as_bool(),
        Some(false)
    );

    // Conservative strip without ledger removes Liftgate's `agy` provider & models while
    // keeping `theme`, `mcp`, `openrouter`, and leaving `permission` untouched without a ledger
    existing.remove("compaction");
    strip_opencode_config(&mut existing);
    assert!(existing.get("model").is_none());
    assert!(existing.get("small_model").is_none());
    assert!(existing.get("provider").is_none());
    assert!(existing["providers"].get("openrouter").is_some());
    assert_eq!(existing["permission"]["doom_loop"].as_str(), Some("ask"));
}

#[test]
fn opencode_no_bypass_preserves_user_execute_deny_object_wildcard_and_rule_order() {
    let base_params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-4-hydrogen-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: Vec::new(),
    };

    // 1. `--no-bypass` (with Code Mode enabled) keeps pre-existing `execute: "deny"` AND
    // non-lexicographical rule order (`"bash*": "allow"` before `"bash": "deny"`).
    let raw_input = r#"{
            "permission": {
                "bash*": "allow",
                "bash": "deny",
                "execute": "deny"
            }
        }"#;
    let mut map: Map<String, Value> = serde_json::from_str(raw_input).expect("valid json input");
    merged_opencode_config_into(&mut map, &base_params, false);
    let reserialized = serde_json::to_string(&Value::Object(map)).expect("serialize");
    let reparsed: Map<String, Value> = serde_json::from_str(&reserialized).expect("reparse");
    let perm_obj = reparsed["permission"].as_object().expect("permission map");
    let keys: Vec<&str> = perm_obj.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["bash*", "bash", "execute"]);
    assert_eq!(perm_obj["execute"].as_str(), Some("deny"));
    assert_eq!(
        reparsed["experimental"]["full_codemode"].as_bool(),
        Some(true)
    );

    // Explicit caller `"experimental": { "full_codemode": false }` opt-out is preserved when
    // `no_code_mode == false`.
    let mut explicit_partial: Map<String, Value> = serde_json::from_value(json!({
        "experimental": {
            "full_codemode": false,
            "subagent_depth": 2
        }
    }))
    .expect("json object");
    merged_opencode_config_into(&mut explicit_partial, &base_params, true);
    assert_eq!(
        explicit_partial["experimental"]["full_codemode"].as_bool(),
        Some(false)
    );
    assert_eq!(
        explicit_partial["experimental"]["subagent_depth"].as_u64(),
        Some(2)
    );

    // 2. `--no-bypass --no-code-mode` preserves object-form `permission["*"]` AND rule order,
    // appending `"execute": "deny"` at the end for last-match-wins evaluation.
    let wildcard_obj_input = r#"{
            "permission": {
                "*": { "*.env": "deny", "*": "ask" },
                "bash*": "allow",
                "bash": "deny"
            }
        }"#;
    let mut map_both: Map<String, Value> =
        serde_json::from_str(wildcard_obj_input).expect("valid wildcard object input");
    let mut params_both = base_params.clone();
    params_both.no_code_mode = true;
    merged_opencode_config_into(&mut map_both, &params_both, false);
    let serialized_both =
        serde_json::to_string_pretty(&Value::Object(map_both)).expect("serialize pretty");
    let reparsed_both: Map<String, Value> =
        serde_json::from_str(&serialized_both).expect("reparse");
    let perm_both = reparsed_both["permission"]
        .as_object()
        .expect("permission map");
    let keys_both: Vec<&str> = perm_both.keys().map(String::as_str).collect();
    assert_eq!(keys_both, vec!["*", "bash*", "bash", "execute"]);
    assert_eq!(perm_both["*"], json!({ "*.env": "deny", "*": "ask" }));
    assert_eq!(perm_both["execute"].as_str(), Some("deny"));
}

#[test]
fn opencode_uninstall_leaves_unmanaged_files_unchanged_and_reverts_only_managed_fields() {
    let tmp = std::env::temp_dir().join(format!(
        "liftgate-oc-uninstall-test-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&tmp).expect("create temp dir");
    let state_paths = StatePaths::resolve(Some(tmp.join("state"))).expect("state paths");

    // 1. Unmanaged handwritten `opencode.json` (even with custom `provider.agy`, `execute: deny`,
    // and JSONC comments) MUST remain byte-for-byte untouched on `--uninstall`.
    let unmanaged_path = tmp.join("unmanaged-opencode.json");
    let handwritten = "{\n  // Handwritten user OpenCode config\n  \"model\": \"agy/custom\",\n  \"provider\": {\n    \"agy\": {\n      \"npm\": \"@ai-sdk/openai-compatible\",\n      \"name\": \"My Custom Gateway\",\n      \"options\": { \"baseURL\": \"http://127.0.0.1:4000/v1\" }\n    }\n  },\n  \"permission\": {\n    \"*\": \"allow\",\n    \"execute\": \"deny\",\n  }\n}\n";
    fs::write(&unmanaged_path, handwritten).expect("write handwritten config");

    let removed_no_state =
        uninstall_opencode_config(&unmanaged_path).expect("uninstall without state");
    assert!(!removed_no_state);
    assert_eq!(
        fs::read_to_string(&unmanaged_path).expect("read after no-state uninstall"),
        handwritten
    );

    let removed_with_state =
        uninstall_opencode_config_with_state(Some(&state_paths), &unmanaged_path)
            .expect("uninstall with state");
    assert!(!removed_with_state);
    assert_eq!(
        fs::read_to_string(&unmanaged_path).expect("read after state uninstall"),
        handwritten
    );

    // 2. Managed file with pre-existing user `execute: "deny"` and `bash*` -> `bash` order:
    // syncing with `--no-code-mode` and then uninstalling restores `execute: "deny"` while
    // removing Liftgate's `"*": "allow"`, `provider.agy`, `model`, and `small_model`, and
    // preserves any user-modified `model` if edited after sync.
    let managed_path = tmp.join("managed-opencode.json");
    let initial_user = r#"{
  "theme": "catppuccin",
  "permission": {
    "bash*": "allow",
    "bash": "deny",
    "execute": "deny"
  }
}
"#;
    fs::write(&managed_path, initial_user).expect("write initial user config");
    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-4-hydrogen-low".to_owned(),
        no_bypass: false,
        no_code_mode: true,
        discovered_models: Vec::new(),
    };
    write_or_update_opencode_config_with_state(Some(&state_paths), &managed_path, &params, false)
        .expect("sync managed opencode config");

    // Simulate user manually switching `"model"` to `"openrouter/deepseek-r1"` after sync
    let mut edited = read_existing_opencode_json_object(&managed_path).expect("read synced");
    edited.insert(
        "model".to_owned(),
        Value::String("openrouter/deepseek-r1".to_owned()),
    );
    fs::write(
        &managed_path,
        serde_json::to_string_pretty(&Value::Object(edited)).expect("serialize edited"),
    )
    .expect("save user edit");

    let uninstalled = uninstall_opencode_config_with_state(Some(&state_paths), &managed_path)
        .expect("uninstall managed config");
    assert!(uninstalled);
    let after = read_existing_opencode_json_object(&managed_path).expect("read after uninstall");
    assert_eq!(
        after.get("theme").and_then(Value::as_str),
        Some("catppuccin")
    );
    // User's edited `model` preserved; Liftgate's untouched `small_model` and `provider.agy` removed!
    assert_eq!(
        after.get("model").and_then(Value::as_str),
        Some("openrouter/deepseek-r1")
    );
    assert!(after.get("small_model").is_none());
    assert!(after.get("provider").is_none());
    assert!(after.get("experimental").is_none());
    // User's original `execute: "deny"` and `bash*` -> `bash` order preserved, Liftgate's `*` removed!
    let after_perms = after["permission"].as_object().expect("permission object");
    let after_perm_keys: Vec<&str> = after_perms.keys().map(String::as_str).collect();
    assert_eq!(after_perm_keys, vec!["bash*", "bash", "execute"]);
    assert_eq!(after_perms["execute"].as_str(), Some("deny"));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn opencode_uninstall_restores_previous_provider_unless_user_changed_it()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = std::env::temp_dir().join(format!(
        "liftgate-oc-provider-test-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&tmp)?;
    let state_paths = StatePaths::resolve(Some(tmp.join("state")))?;
    let original = json!({
        "name": "User gateway",
        "options": { "baseURL": "https://gateway.example.invalid/v1", "apiKey": "test-only" }
    });
    let replacement = json!({ "name": "Edited after sync" });
    let mut params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "generated-test-key".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-4-hydrogen-low".to_owned(),
        no_bypass: true,
        no_code_mode: false,
        discovered_models: Vec::new(),
    };

    for (name, before, user_change, expected) in [
        (
            "restore",
            Some(original.clone()),
            None,
            Some(original.clone()),
        ),
        (
            "edit",
            Some(original.clone()),
            Some(Some(replacement.clone())),
            Some(replacement),
        ),
        ("delete", Some(original.clone()), Some(None), None),
        ("absent", None, None, None),
        ("null", Some(Value::Null), None, Some(Value::Null)),
        ("legacy-ledger", Some(original), None, None),
    ] {
        let path = tmp.join(format!("{name}.json"));
        let mut provider = Map::new();
        provider.insert("other".to_owned(), json!({ "name": "Other provider" }));
        if let Some(before) = before {
            provider.insert(OPENCODE_PROVIDER_ID.to_owned(), before);
        }
        fs::write(&path, serde_json::to_vec(&json!({ "provider": provider }))?)?;
        write_or_update_opencode_config_with_state(Some(&state_paths), &path, &params, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(state_paths.config_sync_ledger_file())?
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A second sync must keep the original provider as the baseline, not the
        // provider generated by the first sync.
        params.api_key.push('x');
        write_or_update_opencode_config_with_state(Some(&state_paths), &path, &params, false)?;
        if let Some(user_change) = user_change {
            let mut config = read_existing_opencode_json_object(&path)?;
            if let Some(Value::Object(providers)) = config.get_mut("provider") {
                if let Some(edited) = user_change {
                    providers.insert(OPENCODE_PROVIDER_ID.to_owned(), edited);
                } else {
                    let _ = remove_key_preserving_order(providers, OPENCODE_PROVIDER_ID);
                }
            }
            fs::write(&path, serde_json::to_vec(&config)?)?;
        }
        if name == "legacy-ledger" {
            let ledger_path = state_paths.config_sync_ledger_file();
            let mut ledger: Value = serde_json::from_slice(&fs::read(&ledger_path)?)?;
            let key = ledger_key_for_path(&path);
            if let Some(Value::Object(record)) = ledger
                .get_mut("opencode")
                .and_then(|entries| entries.get_mut(&key))
            {
                let _ = remove_key_preserving_order(record, "previous_provider_agy");
            }
            write_file_atomic(&ledger_path, &serde_json::to_vec(&ledger)?, true)?;
        }
        assert!(uninstall_opencode_config_with_state(
            Some(&state_paths),
            &path
        )?);
        let config = read_existing_opencode_json_object(&path)?;
        assert_eq!(
            config
                .get("provider")
                .and_then(|p| p.get(OPENCODE_PROVIDER_ID)),
            expected.as_ref(),
            "{name}",
        );
        assert_eq!(
            config.get("provider").and_then(|p| p.get("other")),
            Some(&json!({ "name": "Other provider" })),
        );
        assert!(!state_paths.config_sync_ledger_file().exists());
    }
    fs::remove_dir_all(tmp)?;
    Ok(())
}

#[test]
fn parse_opencode_env_config_content_accepts_jsonc_and_rejects_malformed_or_non_object() {
    assert!(
        parse_opencode_env_config_content(None)
            .expect("unset env")
            .is_empty()
    );
    assert!(
        parse_opencode_env_config_content(Some("  // comment only\n /* block */ "))
            .expect("comment-only env")
            .is_empty()
    );

    let jsonc = "\u{feff}{\n  // Preserve URLs with // and /* inside strings\n  \"provider\": {\n    \"custom\": {\n      \"options\": { \"baseURL\": \"http://127.0.0.1:9000/v1/*keep*/\", },\n    },\n  },\n  /* Multi-line\n     comment */\n  \"permission\": {\n    \"bash*\": \"allow\",\n    \"bash\": \"deny\",\n  },\n}";
    let parsed = parse_opencode_env_config_content(Some(jsonc)).expect("valid JSONC object");
    assert_eq!(
        parsed["provider"]["custom"]["options"]["baseURL"].as_str(),
        Some("http://127.0.0.1:9000/v1/*keep*/")
    );
    let perm_keys: Vec<&str> = parsed["permission"]
        .as_object()
        .expect("permission object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(perm_keys, vec!["bash*", "bash"]);

    assert!(matches!(
        parse_opencode_env_config_content(Some("{ broken json")),
        Err(CliError::InvalidEnvConfigJson {
            var: "OPENCODE_CONFIG_CONTENT",
            ..
        })
    ));
    assert!(matches!(
        parse_opencode_env_config_content(Some("{ /* unclosed comment }")),
        Err(CliError::InvalidEnvConfigJson {
            var: "OPENCODE_CONFIG_CONTENT",
            ..
        })
    ));
    assert!(matches!(
        parse_opencode_env_config_content(Some("[\"not\", \"an\", \"object\"]")),
        Err(CliError::EnvConfigNotObject {
            var: "OPENCODE_CONFIG_CONTENT",
        })
    ));
}

#[test]
fn omp_global_models_and_config_sync_and_uninstall_restore_displaced_agy_provider() {
    let tmp_root =
        std::env::temp_dir().join(format!("liftgate-omp-global-test-{}", uuid::Uuid::new_v4()));
    let state_paths = StatePaths::resolve(Some(tmp_root.join("state"))).unwrap();
    let agent_dir = tmp_root.join("omp-agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let paths = OmpResolvedPaths {
        config_path: agent_dir.join("config.yml"),
        auxiliary_path: agent_dir.join("models.yml"),
    };

    let initial_models = "\
# User custom models.yml
providers:
  ollama:
    baseUrl: http://127.0.0.1:11434/v1
    api: openai-completions
  agy:
    baseUrl: https://custom-agy.internal/v1
    apiKey: user-original-secret
    api: openai-responses
";
    let initial_config = "\
# User global config
theme: nord
modelRoles:
  default: anthropic/claude-sonnet-4-6
  advisor: openai/o3
tools:
  approvalMode: write
";
    fs::write(&paths.auxiliary_path, initial_models).unwrap();
    fs::write(&paths.config_path, initial_config).unwrap();

    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: false,
        no_code_mode: false,
        discovered_models: vec![
            DiscoveredModel {
                id: "gemini-4-carbon-high".to_owned(),
                display_name: "Gemini 4 Carbon (High)".to_owned(),
                context_window: 500_000,
                auto_compact_token_limit: 425_000,
            },
            DiscoveredModel {
                id: "claude-opus-4-6-thinking".to_owned(),
                display_name: "Claude Opus 4.6 (Thinking)".to_owned(),
                context_window: 200_000,
                auto_compact_token_limit: 170_000,
            },
        ],
    };

    // Sync twice to verify idempotency and preservation of the pre-sync displaced `agy:` block.
    write_or_update_omp_config_with_state(Some(&state_paths), true, &paths, &params, false)
        .unwrap();
    write_or_update_omp_config_with_state(Some(&state_paths), true, &paths, &params, false)
        .unwrap();

    let synced_models = fs::read_to_string(&paths.auxiliary_path).unwrap();
    let synced_cfg = fs::read_to_string(&paths.config_path).unwrap();
    assert!(synced_models.contains("# User custom models.yml"));
    assert!(synced_models.contains("http://127.0.0.1:11434/v1"));
    assert!(synced_models.contains("http://127.0.0.1:8787/v1"));
    assert!(synced_models.contains("preferWebsockets: false"));
    assert!(synced_models.contains("contextWindow: 500000"));
    assert!(synced_models.contains("contextWindow: 200000"));
    assert!(!synced_models.contains("user-original-secret"));

    assert!(synced_cfg.contains("theme: nord"));
    assert!(synced_cfg.contains("default: agy/gemini-4-carbon-high"));
    assert!(synced_cfg.contains("smol: agy/gemini-3.8-flash-low"));
    assert!(synced_cfg.contains("advisor: openai/o3"));
    assert!(synced_cfg.contains("approvalMode: yolo"));

    let removed = uninstall_omp_config_with_state(Some(&state_paths), true, &paths).unwrap();
    assert!(removed);

    let restored_models = fs::read_to_string(&paths.auxiliary_path).unwrap();
    let restored_cfg = fs::read_to_string(&paths.config_path).unwrap();
    assert!(restored_models.contains("https://custom-agy.internal/v1"));
    assert!(restored_models.contains("user-original-secret"));
    assert!(restored_models.contains("http://127.0.0.1:11434/v1"));
    assert!(restored_cfg.contains("theme: nord"));
    assert!(restored_cfg.contains("default: anthropic/claude-sonnet-4-6"));
    assert!(restored_cfg.contains("advisor: openai/o3"));
    assert!(restored_cfg.contains("approvalMode: write"));
    assert!(!restored_cfg.contains("agy/gemini-4-carbon-high"));

    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn omp_project_local_sync_writes_provider_ts_preserves_extensions_and_cleans_up() {
    let tmp_root =
        std::env::temp_dir().join(format!("liftgate-omp-local-test-{}", uuid::Uuid::new_v4()));
    let state_paths = StatePaths::resolve(Some(tmp_root.join("state"))).unwrap();
    let proj_dir = tmp_root.join("proj");
    fs::create_dir_all(proj_dir.join(".omp")).unwrap();
    let paths = resolve_omp_paths(false, Some(&proj_dir)).unwrap();
    fs::write(&paths.config_path, "extensions:\n  - ./.omp/user-ext.ts\n").unwrap();

    let params = HarnessSyncParams {
        gateway_base_url: "http://127.0.0.1:8787".to_owned(),
        api_key: "local-agy".to_owned(),
        primary_model: "gemini-4-carbon-high".to_owned(),
        fast_model: "gemini-3.8-flash-low".to_owned(),
        no_bypass: true,
        no_code_mode: true,
        discovered_models: Vec::new(),
    };

    write_or_update_omp_config_with_state(Some(&state_paths), false, &paths, &params, false)
        .unwrap();
    let cfg_text = fs::read_to_string(&paths.config_path).unwrap();
    let ext_ts = fs::read_to_string(&paths.auxiliary_path).unwrap();
    assert!(cfg_text.contains("./.omp/user-ext.ts"));
    assert!(cfg_text.contains(OMP_LOCAL_EXTENSION_REL_PATH));
    assert!(cfg_text.contains("approvalMode: always-ask"));
    assert!(cfg_text.contains("py: false"));
    assert!(cfg_text.contains("js: false"));
    assert!(ext_ts.contains("pi.registerProvider(\"agy\""));

    let removed = uninstall_omp_config_with_state(Some(&state_paths), false, &paths).unwrap();
    assert!(removed);
    assert!(!paths.auxiliary_path.exists());
    let restored_cfg = fs::read_to_string(&paths.config_path).unwrap();
    assert!(restored_cfg.contains("./.omp/user-ext.ts"));
    assert!(!restored_cfg.contains(OMP_LOCAL_EXTENSION_REL_PATH));
    assert!(!restored_cfg.contains("modelRoles:"));

    let _ = fs::remove_dir_all(&tmp_root);
}
