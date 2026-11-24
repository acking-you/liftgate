//! Persistent harness configuration coordinator (`lg config`), delegating pure schemas,
//! structure-preserving YAML/TOML/JSON editors, and ledger restoration to `liftgate-harness`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::cli::{
    ConfigArgs, ConfigTarget, PLACEHOLDER_API_KEY, ServeArgs, normalize_optional_api_key,
};
use crate::daemon::{ensure_gateway_ready, resolve_client_api_key};
use crate::error::CliError;
use crate::state::{StatePaths, format_connect_url};

pub use liftgate_harness::claude::{
    build_claude_settings_map, merged_claude_settings_into, strip_claude_settings,
};
pub use liftgate_harness::codex::{
    CodexSyncRequest, merge_codex_config_toml, merge_codex_config_toml_with_mode,
    merge_isolated_codex_app_config_toml, strip_codex_config_toml,
};
pub use liftgate_harness::fs_util::strip_jsonc_to_json;
pub use liftgate_harness::omp::{
    OMP_EPHEMERAL_API_KEY_ENV, OMP_LOCAL_EXTENSION_REL_PATH, OMP_MANAGED_EXTENSION_SENTINEL,
    OMP_PROVIDER_ID, OmpResolvedPaths, build_omp_ephemeral_overlay_yaml,
    build_omp_provider_extension_ts, build_omp_provider_json, extract_omp_yaml_extensions,
    has_omp_agy_provider_yaml, inspect_omp_default_model, render_omp_provider_agy_yaml_block,
};
pub use liftgate_harness::opencode::{
    OPENCODE_PROVIDER_ID, build_opencode_config_map, has_liftgate_managed_opencode_provider,
    merged_opencode_config_into, strip_opencode_config,
};
pub use liftgate_harness::{
    DiscoveredModel, HarnessSyncParams, OmpCodeModePolicy, resolve_fast_model,
    strip_agy_provider_prefix,
};

#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncTargetResult {
    pub target: &'static str,
    pub action: &'static str,
    pub config_path: PathBuf,
    pub catalog_path: Option<PathBuf>,
    pub primary_model: Option<String>,
    pub fast_model: Option<String>,
    pub rendered_preview: Option<String>,
    pub project_untrusted_codex_note: bool,
    pub require_env_key_warn: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncOutcome {
    pub global: bool,
    pub results: Vec<ConfigSyncTargetResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfigSyncInspection {
    pub global: bool,
    pub claude_path: PathBuf,
    pub claude_exists: bool,
    pub claude_managed: bool,
    pub claude_model: Option<String>,
    pub claude_base_url: Option<String>,
    pub codex_path: PathBuf,
    pub codex_catalog_path: PathBuf,
    pub codex_exists: bool,
    pub codex_managed: bool,
    pub codex_model: Option<String>,
    pub opencode_path: PathBuf,
    pub opencode_exists: bool,
    pub opencode_managed: bool,
    pub opencode_model: Option<String>,
    pub omp_path: PathBuf,
    pub omp_auxiliary_path: PathBuf,
    pub omp_exists: bool,
    pub omp_managed: bool,
    pub omp_model: Option<String>,
}

pub fn resolve_claude_config_path(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<PathBuf, CliError> {
    Ok(liftgate_harness::claude::resolve_claude_config_path(
        global,
        project_dir,
    )?)
}

pub fn write_or_update_claude_config(
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    Ok(liftgate_harness::claude::write_or_update_claude_config(
        target_path,
        params,
        print_only,
    )?)
}

pub fn uninstall_claude_config(target_path: &Path) -> Result<bool, CliError> {
    Ok(liftgate_harness::claude::uninstall_claude_config(
        target_path,
    )?)
}

pub fn resolve_opencode_config_path(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<PathBuf, CliError> {
    Ok(liftgate_harness::opencode::resolve_opencode_config_path(
        global,
        project_dir,
    )?)
}

pub fn write_or_update_opencode_config(
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    write_or_update_opencode_config_with_state(None, target_path, params, print_only)
}

pub fn write_or_update_opencode_config_with_state(
    state_paths: Option<&StatePaths>,
    target_path: &Path,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    let ledger_path = match state_paths {
        Some(sp) => {
            if !print_only {
                sp.ensure_root_dir()?;
            }
            Some(sp.config_sync_ledger_file())
        }
        None => None,
    };
    Ok(
        liftgate_harness::opencode::write_or_update_opencode_config_with_ledger(
            ledger_path.as_deref(),
            target_path,
            params,
            print_only,
        )?,
    )
}

pub fn uninstall_opencode_config(target_path: &Path) -> Result<bool, CliError> {
    uninstall_opencode_config_with_state(None, target_path)
}

pub fn uninstall_opencode_config_with_state(
    state_paths: Option<&StatePaths>,
    target_path: &Path,
) -> Result<bool, CliError> {
    let ledger_path = state_paths.map(StatePaths::config_sync_ledger_file);
    Ok(
        liftgate_harness::opencode::uninstall_opencode_config_with_ledger(
            ledger_path.as_deref(),
            target_path,
        )?,
    )
}

pub fn parse_opencode_env_config_content(
    raw: Option<&str>,
) -> Result<Map<String, Value>, CliError> {
    Ok(liftgate_harness::opencode::parse_opencode_env_config_content(raw)?)
}

pub fn resolve_omp_paths(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<OmpResolvedPaths, CliError> {
    Ok(liftgate_harness::omp::resolve_omp_paths(
        global,
        project_dir,
    )?)
}

pub fn merge_omp_models_yaml(
    existing: &str,
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
    target_path: &Path,
) -> Result<(String, Option<String>, String, Option<String>), CliError> {
    Ok(liftgate_harness::omp::merge_omp_models_yaml(
        existing,
        params,
        api_key_override,
        target_path,
    )?)
}

pub fn write_or_update_omp_config(
    global: bool,
    paths: &OmpResolvedPaths,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    write_or_update_omp_config_with_state(None, global, paths, params, print_only)
}

pub fn write_or_update_omp_config_with_state(
    state_paths: Option<&StatePaths>,
    global: bool,
    paths: &OmpResolvedPaths,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, CliError> {
    let ledger_path = match state_paths {
        Some(sp) => {
            if !print_only {
                sp.ensure_root_dir()?;
            }
            Some(sp.config_sync_ledger_file())
        }
        None => None,
    };
    Ok(
        liftgate_harness::omp::write_or_update_omp_config_with_ledger(
            ledger_path.as_deref(),
            global,
            paths,
            params,
            print_only,
        )?,
    )
}

pub fn uninstall_omp_config(global: bool, paths: &OmpResolvedPaths) -> Result<bool, CliError> {
    uninstall_omp_config_with_state(None, global, paths)
}

pub fn uninstall_omp_config_with_state(
    state_paths: Option<&StatePaths>,
    global: bool,
    paths: &OmpResolvedPaths,
) -> Result<bool, CliError> {
    let ledger_path = state_paths.map(StatePaths::config_sync_ledger_file);
    Ok(liftgate_harness::omp::uninstall_omp_config_with_ledger(
        ledger_path.as_deref(),
        global,
        paths,
    )?)
}

pub fn build_offline_codex_catalog_bytes_with_mode(
    discovered: &[DiscoveredModel],
    primary_model: &str,
    tool_mode: Option<&str>,
) -> Result<Vec<u8>, CliError> {
    Ok(
        liftgate_harness::codex::build_offline_codex_catalog_bytes_with_mode(
            discovered,
            primary_model,
            tool_mode,
        )?,
    )
}

pub fn resolve_codex_paths(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf), CliError> {
    Ok(liftgate_harness::codex::resolve_codex_paths(
        global,
        project_dir,
    )?)
}

pub fn write_or_update_codex_config(req: &CodexSyncRequest<'_>) -> Result<String, CliError> {
    Ok(liftgate_harness::codex::write_or_update_codex_config(req)?)
}

pub fn uninstall_codex_config(config_path: &Path, catalog_path: &Path) -> Result<bool, CliError> {
    Ok(liftgate_harness::codex::uninstall_codex_config(
        config_path,
        catalog_path,
    )?)
}

pub fn inspect_config_sync(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<ConfigSyncInspection, CliError> {
    let claude_path = resolve_claude_config_path(global, project_dir)?;
    let claude_exists = claude_path.is_file();
    let claude_obj = if claude_exists {
        liftgate_harness::fs_util::read_existing_json_object(&claude_path).unwrap_or_default()
    } else {
        Map::new()
    };
    let claude_base_url = claude_obj
        .get("env")
        .and_then(Value::as_object)
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let claude_managed = claude_base_url.is_some() || claude_obj.contains_key("modelPicker");
    let claude_model = claude_obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let (codex_path, codex_catalog_path) = resolve_codex_paths(global, project_dir)?;
    let codex_exists = codex_path.is_file();
    let codex_text = if codex_exists {
        fs::read_to_string(&codex_path).unwrap_or_default()
    } else {
        String::new()
    };
    let codex_managed = codex_text.contains("[model_providers.agy]")
        || codex_text.contains("model_provider = \"agy\"");
    let codex_model = liftgate_harness::codex::inspect_codex_model(&codex_text);

    let opencode_path = resolve_opencode_config_path(global, project_dir)?;
    let opencode_exists = opencode_path.is_file();
    let opencode_obj = if opencode_exists {
        liftgate_harness::opencode::read_existing_opencode_json_object(&opencode_path)
            .unwrap_or_default()
    } else {
        Map::new()
    };
    let opencode_managed = has_liftgate_managed_opencode_provider(&opencode_obj);
    let opencode_model = opencode_obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let omp_paths = resolve_omp_paths(global, project_dir)?;
    let omp_exists = omp_paths.config_path.is_file() || omp_paths.auxiliary_path.is_file();
    let omp_cfg_text = fs::read_to_string(&omp_paths.config_path).unwrap_or_default();
    let omp_aux_text = fs::read_to_string(&omp_paths.auxiliary_path).unwrap_or_default();
    let omp_model = inspect_omp_default_model(&omp_cfg_text);
    let omp_managed = omp_model
        .as_deref()
        .is_some_and(|m| m.starts_with(&format!("{OMP_PROVIDER_ID}/")))
        || (global && has_omp_agy_provider_yaml(&omp_aux_text))
        || (!global && omp_aux_text.starts_with(OMP_MANAGED_EXTENSION_SENTINEL));

    Ok(ConfigSyncInspection {
        global,
        claude_path,
        claude_exists,
        claude_managed,
        claude_model,
        claude_base_url,
        codex_path,
        codex_catalog_path,
        codex_exists,
        codex_managed,
        codex_model,
        opencode_path,
        opencode_exists,
        opencode_managed,
        opencode_model,
        omp_path: omp_paths.config_path,
        omp_auxiliary_path: omp_paths.auxiliary_path,
        omp_exists,
        omp_managed,
        omp_model,
    })
}

pub async fn execute_config_sync(
    state_paths: &StatePaths,
    args: &ConfigArgs,
) -> Result<ConfigSyncOutcome, CliError> {
    let want_claude = matches!(args.target, ConfigTarget::All | ConfigTarget::Claude);
    let want_codex = matches!(args.target, ConfigTarget::All | ConfigTarget::Codex);
    let want_opencode = matches!(args.target, ConfigTarget::All | ConfigTarget::Opencode);
    let want_omp = matches!(args.target, ConfigTarget::All | ConfigTarget::Omp);
    let mut results = Vec::with_capacity(4);

    if args.uninstall {
        if want_claude {
            let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
            let removed = uninstall_claude_config(&claude_path)?;
            results.push(ConfigSyncTargetResult {
                target: "claude",
                action: if removed { "removed" } else { "not_present" },
                config_path: claude_path,
                catalog_path: None,
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        if want_codex {
            let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
            let removed = uninstall_codex_config(&codex_cfg, &codex_cat)?;
            results.push(ConfigSyncTargetResult {
                target: "codex",
                action: if removed { "removed" } else { "not_present" },
                config_path: codex_cfg,
                catalog_path: Some(codex_cat),
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        if want_opencode {
            let opencode_path = resolve_opencode_config_path(args.global, args.dir.as_deref())?;
            let removed = uninstall_opencode_config_with_state(Some(state_paths), &opencode_path)?;
            results.push(ConfigSyncTargetResult {
                target: "opencode",
                action: if removed { "removed" } else { "not_present" },
                config_path: opencode_path,
                catalog_path: None,
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        if want_omp {
            let omp_paths = resolve_omp_paths(args.global, args.dir.as_deref())?;
            let removed =
                uninstall_omp_config_with_state(Some(state_paths), args.global, &omp_paths)?;
            results.push(ConfigSyncTargetResult {
                target: "omp",
                action: if removed { "removed" } else { "not_present" },
                config_path: omp_paths.config_path,
                catalog_path: Some(omp_paths.auxiliary_path),
                primary_model: None,
                fast_model: None,
                rendered_preview: None,
                project_untrusted_codex_note: false,
                require_env_key_warn: false,
            });
        }
        return Ok(ConfigSyncOutcome {
            global: args.global,
            results,
        });
    }

    let live_target = if args.offline {
        None
    } else {
        match ensure_gateway_ready(
            state_paths,
            args.listen,
            args.api_key.clone(),
            args.model.as_deref(),
            !args.print,
        )
        .await
        {
            Ok(target) => Some(target),
            Err(_) if args.print => None,
            Err(err) => return Err(err),
        }
    };

    let daemon = state_paths.read_daemon()?;
    let default_serve = ServeArgs::default();
    let resolved_listen = args
        .listen
        .or_else(|| daemon.as_ref().map(|d| d.listen))
        .unwrap_or(default_serve.listen);

    let gateway_base_url = if let Some(t) = &live_target {
        t.client.base_url().to_owned()
    } else {
        format_connect_url(resolved_listen)
    };

    let configured_api_key = if let Some(t) = &live_target {
        normalize_optional_api_key(t.api_key.as_deref())
    } else {
        resolve_client_api_key(args.api_key.as_deref(), daemon.as_ref(), resolved_listen)
    };
    let require_env_key = configured_api_key.is_some();
    let claude_auth_token = configured_api_key.unwrap_or_else(|| PLACEHOLDER_API_KEY.to_owned());

    let primary_model = args
        .model
        .as_deref()
        .map(strip_agy_provider_prefix)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| live_target.as_ref().map(|t| t.default_model.clone()))
        .or_else(|| daemon.as_ref().map(|d| d.default_model.clone()))
        .unwrap_or(default_serve.default_model);

    let discovered_models = if let Some(t) = &live_target {
        t.client.fetch_models().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let fast_model = resolve_fast_model(
        args.fast_model.as_deref().map(strip_agy_provider_prefix),
        &primary_model,
        &discovered_models,
    );

    if want_claude {
        let claude_path = resolve_claude_config_path(args.global, args.dir.as_deref())?;
        let sync_params = HarnessSyncParams {
            gateway_base_url: gateway_base_url.clone(),
            api_key: claude_auth_token.clone(),
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            discovered_models: discovered_models.clone(),
        };
        let rendered = write_or_update_claude_config(&claude_path, &sync_params, args.print)?;
        results.push(ConfigSyncTargetResult {
            target: "claude",
            action: if args.print { "printed" } else { "synced" },
            config_path: claude_path,
            catalog_path: None,
            primary_model: Some(primary_model.clone()),
            fast_model: Some(fast_model.clone()),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: false,
            require_env_key_warn: false,
        });
    }

    if want_codex {
        let (codex_cfg, codex_cat) = resolve_codex_paths(args.global, args.dir.as_deref())?;
        let requested_tool_mode = if args.no_code_mode {
            Some("direct")
        } else {
            Some("code_mode")
        };
        let catalog_bytes = if let Some(t) = &live_target {
            match t
                .client
                .fetch_codex_catalog_json_with_mode(&primary_model, requested_tool_mode)
                .await
            {
                Ok(bytes) => bytes,
                Err(_) => build_offline_codex_catalog_bytes_with_mode(
                    &discovered_models,
                    &primary_model,
                    requested_tool_mode,
                )?,
            }
        } else {
            build_offline_codex_catalog_bytes_with_mode(
                &discovered_models,
                &primary_model,
                requested_tool_mode,
            )?
        };

        let rendered = write_or_update_codex_config(&CodexSyncRequest {
            config_path: &codex_cfg,
            catalog_path: &codex_cat,
            catalog_bytes: &catalog_bytes,
            gateway_base_url: &gateway_base_url,
            primary_model: &primary_model,
            require_env_key,
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            print_only: args.print,
        })?;
        results.push(ConfigSyncTargetResult {
            target: "codex",
            action: if args.print { "printed" } else { "synced" },
            config_path: codex_cfg,
            catalog_path: Some(codex_cat),
            primary_model: Some(primary_model.clone()),
            fast_model: Some(fast_model.clone()),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: !args.global,
            require_env_key_warn: require_env_key,
        });
    }

    if want_opencode {
        let opencode_path = resolve_opencode_config_path(args.global, args.dir.as_deref())?;
        let params = HarnessSyncParams {
            gateway_base_url: gateway_base_url.clone(),
            api_key: claude_auth_token.clone(),
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            discovered_models: discovered_models.clone(),
        };
        let rendered = write_or_update_opencode_config_with_state(
            Some(state_paths),
            &opencode_path,
            &params,
            args.print,
        )?;
        results.push(ConfigSyncTargetResult {
            target: "opencode",
            action: if args.print { "printed" } else { "synced" },
            config_path: opencode_path,
            catalog_path: None,
            primary_model: Some(primary_model.clone()),
            fast_model: Some(fast_model.clone()),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: false,
            require_env_key_warn: false,
        });
    }

    if want_omp {
        let omp_paths = resolve_omp_paths(args.global, args.dir.as_deref())?;
        let params = HarnessSyncParams {
            gateway_base_url,
            api_key: claude_auth_token,
            primary_model: primary_model.clone(),
            fast_model: fast_model.clone(),
            no_bypass: args.no_bypass,
            no_code_mode: args.no_code_mode,
            discovered_models,
        };
        let rendered = write_or_update_omp_config_with_state(
            Some(state_paths),
            args.global,
            &omp_paths,
            &params,
            args.print,
        )?;
        results.push(ConfigSyncTargetResult {
            target: "omp",
            action: if args.print { "printed" } else { "synced" },
            config_path: omp_paths.config_path,
            catalog_path: Some(omp_paths.auxiliary_path),
            primary_model: Some(primary_model),
            fast_model: Some(fast_model),
            rendered_preview: Some(rendered),
            project_untrusted_codex_note: false,
            require_env_key_warn: false,
        });
    }

    Ok(ConfigSyncOutcome {
        global: args.global,
        results,
    })
}

#[cfg(test)]
mod tests;
