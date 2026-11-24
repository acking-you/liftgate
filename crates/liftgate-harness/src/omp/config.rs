//! Structure-preserving Oh My Pi (`omp`) `models.yml` & `config.yml` surgical merge, revert, and inspect.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::HarnessError;
use crate::fs_util::{
    backup_once_if_exists, canonicalize_clean, read_optional_utf8_file, resolve_base_dir,
    user_home_dir, write_file_atomic,
};
use crate::ledger::{
    OmpManagedRecord, ledger_key_for_path, read_config_sync_ledger, write_config_sync_ledger,
};
use crate::omp::provider::{
    build_omp_provider_extension_ts, omp_model_supports_images,
    render_omp_provider_agy_yaml_block_indented,
};
use crate::omp::{
    OMP_FAST_ROLE_KEYS, OMP_LOCAL_EXTENSION_FILENAME, OMP_LOCAL_EXTENSION_REL_PATH,
    OMP_MANAGED_EXTENSION_SENTINEL, OMP_PRIMARY_ROLE_KEYS, OMP_PROVIDER_ID,
};
use crate::yaml_edit::{
    YamlScalar, YamlSubEntry, YamlTopSection, ensure_lf, parse_mapping_section,
    parse_yaml_document, parse_yaml_string_sequence_section, render_and_validate_yaml_document,
    revert_yaml_2level_scalars, split_yaml_key_and_rhs, strip_yaml_inline_comment,
    unquote_yaml_scalar, upsert_yaml_2level_scalars, yaml_quote_scalar,
};
use crate::{HarnessSyncParams, OmpCodeModePolicy, strip_agy_provider_prefix};

/// Resolved filesystem paths for Oh My Pi (`omp`) configuration sync and inspection.
#[derive(Clone, Debug)]
pub struct OmpResolvedPaths {
    /// Primary YAML config (`~/.omp/agent/config.yml` globally, or `<dir>/.omp/config.yml` locally).
    pub config_path: PathBuf,
    /// Provider registration file (`~/.omp/agent/models.yml` globally, or `<dir>/.omp/liftgate-agy-provider.ts` locally).
    pub auxiliary_path: PathBuf,
}

/// Resolves Oh My Pi's active config and provider paths honoring `@oh-my-pi/pi-utils` rules.
pub fn resolve_omp_paths(
    global: bool,
    project_dir: Option<&Path>,
) -> Result<OmpResolvedPaths, HarnessError> {
    if !global {
        let omp_dir = resolve_base_dir(project_dir)?.join(".omp");
        return Ok(OmpResolvedPaths {
            config_path: omp_dir.join("config.yml"),
            auxiliary_path: omp_dir.join(OMP_LOCAL_EXTENSION_FILENAME),
        });
    }

    let omp_profile = std::env::var("OMP_PROFILE").ok();
    let pi_profile = std::env::var("PI_PROFILE").ok();
    let profile = omp_profile
        .as_deref()
        .or(pi_profile.as_deref())
        .and_then(normalize_omp_profile_name);

    let agent_dir = if let Some(explicit) = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        canonicalize_clean(&explicit)
    } else if let Some(cfg_root) = std::env::var_os("PI_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        let root = canonicalize_clean(&cfg_root);
        match &profile {
            Some(prof) => root.join("profiles").join(prof).join("agent"),
            None => root.join("agent"),
        }
    } else {
        let home = user_home_dir()?;
        #[cfg(windows)]
        let base_root = home.join(".omp");
        #[cfg(not(windows))]
        let base_root = {
            let dot_omp = home.join(".omp");
            if dot_omp.exists() {
                dot_omp
            } else {
                let xdg_root = std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| home.join(".config"), PathBuf::from)
                    .join("omp");
                if xdg_root.exists() { xdg_root } else { dot_omp }
            }
        };
        match &profile {
            Some(prof) => base_root.join("profiles").join(prof).join("agent"),
            None => base_root.join("agent"),
        }
    };

    let config_filename = match &profile {
        Some(prof) => format!("config.{prof}.yml"),
        None => "config.yml".to_owned(),
    };
    Ok(OmpResolvedPaths {
        config_path: agent_dir.join(config_filename),
        auxiliary_path: agent_dir.join("models.yml"),
    })
}

fn normalize_omp_profile_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return None;
    }
    let valid = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| trimmed.to_owned())
}

/// Non-destructively merges `providers.agy` into `~/.omp/agent/models.yml`, preserving existing
/// indentation (e.g. 2-space or 4-space), sibling providers, and comments, and validating the
/// resulting YAML before returning `(rendered, previous_agy_block, wrote_agy_block, previous_header_rhs)`.
pub fn merge_omp_models_yaml(
    existing: &str,
    params: &HarnessSyncParams,
    api_key_override: Option<&str>,
    target_path: &Path,
) -> Result<(String, Option<String>, String, Option<String>), HarnessError> {
    let mut doc =
        parse_yaml_document(existing).map_err(|detail| HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        })?;

    if doc.sections.is_empty() && doc.doc_start.is_none() && doc.doc_end.is_none() {
        let agy_block = render_omp_provider_agy_yaml_block_indented(params, api_key_override, 2, 2);
        let rendered = format!("providers:\n{agy_block}");
        return Ok((rendered, None, agy_block, None));
    }

    let mut prev_agy: Option<String> = None;
    let mut prev_header_rhs: Option<String> = None;
    let mut wrote_agy = String::new();
    let mut found_providers = false;

    for sec in &mut doc.sections {
        if sec.key.as_deref() != Some("providers") {
            continue;
        }
        found_providers = true;
        let parsed =
            parse_mapping_section(&sec.lines, doc.default_indent, false).map_err(|detail| {
                HarnessError::UnsupportedYamlLayout {
                    path: target_path.to_path_buf(),
                    detail,
                }
            })?;
        prev_header_rhs = parsed.previous_header_rhs.clone();
        let agy_block = render_omp_provider_agy_yaml_block_indented(
            params,
            api_key_override,
            parsed.child_indent,
            parsed.child_indent,
        );
        wrote_agy.clone_from(&agy_block);

        let mut rebuilt = vec![parsed.format_block_header()];
        let mut inserted = false;
        for child in parsed.children {
            if child.key.as_deref() == Some(OMP_PROVIDER_ID) {
                prev_agy = Some(child.lines.concat());
                if !inserted {
                    rebuilt.push(agy_block.clone());
                    inserted = true;
                }
            } else {
                rebuilt.extend(child.lines);
            }
        }
        if !inserted {
            rebuilt.push(agy_block);
        }
        rebuilt.extend(parsed.trailing_blanks);
        sec.lines = rebuilt;
        break;
    }

    if !found_providers {
        let indent = doc.default_indent.max(2);
        let agy_block =
            render_omp_provider_agy_yaml_block_indented(params, api_key_override, indent, indent);
        wrote_agy.clone_from(&agy_block);
        if let Some(last) = doc.sections.last_mut()
            && last.lines.last().is_some_and(|l| !l.trim().is_empty())
        {
            last.lines.push("\n".to_owned());
        }
        doc.sections.push(YamlTopSection {
            key: Some("providers".to_owned()),
            lines: vec!["providers:\n".to_owned(), agy_block],
        });
    }

    let rendered = render_and_validate_yaml_document(&doc).map_err(|detail| {
        HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        }
    })?;
    Ok((rendered, prev_agy, wrote_agy, prev_header_rhs))
}

/// Reverts Liftgate's `providers.agy` block from `models.yml`, preserving 2-space or 4-space
/// sibling indentation, comments, and prior displaced `agy` blocks, and validating the result.
pub fn revert_omp_models_yaml(
    existing: &str,
    wrote_agy_yaml: Option<&str>,
    previous_agy_yaml: Option<&str>,
    previous_header_rhs: Option<&str>,
    target_path: &Path,
) -> Result<(String, bool), HarnessError> {
    if existing.trim_start_matches('\u{feff}').trim().is_empty() {
        return Ok((String::new(), false));
    }
    let mut doc =
        parse_yaml_document(existing).map_err(|detail| HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        })?;
    let mut changed = false;
    let mut keep_sections: Vec<YamlTopSection> = Vec::with_capacity(doc.sections.len());

    for mut sec in doc.sections.drain(..) {
        if sec.key.as_deref() != Some("providers") {
            keep_sections.push(sec);
            continue;
        }
        let parsed =
            parse_mapping_section(&sec.lines, doc.default_indent, false).map_err(|detail| {
                HarnessError::UnsupportedYamlLayout {
                    path: target_path.to_path_buf(),
                    detail,
                }
            })?;
        let block_header = parsed.format_block_header();
        let mut kept_children: Vec<YamlSubEntry> = Vec::new();
        for child in parsed.children {
            if child.key.as_deref() == Some(OMP_PROVIDER_ID) {
                let current_raw = child.lines.concat();
                let matches_written = match wrote_agy_yaml {
                    Some(wrote) => current_raw.trim() == wrote.trim(),
                    None => {
                        current_raw.contains("api: openai-responses")
                            && current_raw.contains("preferWebsockets: false")
                    }
                };
                if matches_written {
                    changed = true;
                    if let Some(prev) = previous_agy_yaml {
                        kept_children.push(YamlSubEntry {
                            key: Some(OMP_PROVIDER_ID.to_owned()),
                            lines: prev.split_inclusive('\n').map(ensure_lf).collect(),
                        });
                    }
                } else {
                    kept_children.push(child);
                }
            } else {
                kept_children.push(child);
            }
        }

        let any_real_provider = kept_children.iter().any(|c| c.key.is_some());
        if any_real_provider {
            let mut rebuilt = vec![block_header];
            for c in kept_children {
                rebuilt.extend(c.lines);
            }
            rebuilt.extend(parsed.trailing_blanks);
            sec.lines = rebuilt;
            keep_sections.push(sec);
        } else if changed {
            if let Some(prev_rhs) = previous_header_rhs
                && matches!(prev_rhs, "{}" | "null" | "~")
            {
                let head = match &parsed.header_comment {
                    Some(comment) => format!("providers: {prev_rhs} {comment}\n"),
                    None => format!("providers: {prev_rhs}\n"),
                };
                let mut rebuilt = vec![head];
                rebuilt.extend(parsed.trailing_blanks);
                sec.lines = rebuilt;
                keep_sections.push(sec);
            }
        } else {
            keep_sections.push(sec);
        }
    }

    doc.sections = keep_sections;
    if !changed {
        return Ok((existing.to_owned(), false));
    }
    let rendered = render_and_validate_yaml_document(&doc).map_err(|detail| {
        HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        }
    })?;
    Ok((rendered, true))
}

/// Builds an ephemeral `omp-overlay-<uuid>.yml` document loaded via `PI_CONFIG_FILES` for `lg omp`.
#[must_use]
pub fn build_omp_ephemeral_overlay_yaml(
    params: &HarnessSyncParams,
    code_mode: OmpCodeModePolicy,
    provider_ext_path: &Path,
    inherited_extensions: &[String],
) -> String {
    let mut merged = String::new();
    let mut ext_list: Vec<String> = Vec::with_capacity(inherited_extensions.len() + 1);
    for item in inherited_extensions {
        let trimmed = item.trim();
        if !trimmed.is_empty() && !ext_list.iter().any(|existing| existing == trimmed) {
            ext_list.push(trimmed.to_owned());
        }
    }
    let provider_str = provider_ext_path.to_string_lossy().replace('\\', "/");
    if !ext_list.iter().any(|existing| existing == &provider_str) {
        ext_list.push(provider_str);
    }
    let _ = merge_omp_config_yaml_with_record(
        &mut merged,
        params,
        code_mode,
        Some(&ext_list),
        Path::new("omp-overlay.yml"),
    );
    merged
}

/// Extracts any `extensions:` string entries declared in an existing `omp` YAML or JSON config file,
/// supporting YAML flow lists (`[./.omp/ext.ts, './b.ts']`) and block `- item` sequences.
#[must_use]
pub fn extract_omp_yaml_extensions(yaml_text: &str) -> Vec<String> {
    let trimmed_bom = yaml_text.strip_prefix('\u{feff}').unwrap_or(yaml_text);
    if trimmed_bom.trim_start().starts_with('{')
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed_bom)
        && let Some(Value::Array(arr)) = map.get("extensions")
    {
        return arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }

    let Ok(doc) = parse_yaml_document(trimmed_bom) else {
        return Vec::new();
    };
    for sec in doc.sections {
        if sec.key.as_deref() == Some("extensions")
            && let Ok((items, _)) =
                parse_yaml_string_sequence_section(&sec.lines, doc.default_indent)
        {
            return items;
        }
    }
    Vec::new()
}

fn merge_omp_config_yaml_with_record(
    yaml_buf: &mut String,
    params: &HarnessSyncParams,
    code_mode: OmpCodeModePolicy,
    explicit_extensions: Option<&[String]>,
    target_path: &Path,
) -> Result<OmpManagedRecord, HarnessError> {
    let mut doc =
        parse_yaml_document(yaml_buf).map_err(|detail| HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        })?;

    let primary_id = strip_agy_provider_prefix(&params.primary_model);
    let fast_id = strip_agy_provider_prefix(&params.fast_model);
    let primary_sel = format!("{OMP_PROVIDER_ID}/{primary_id}");
    let fast_sel = format!("{OMP_PROVIDER_ID}/{fast_id}");

    let mut desired_roles: Vec<(&str, YamlScalar)> = Vec::with_capacity(10);
    for role in OMP_PRIMARY_ROLE_KEYS {
        desired_roles.push((*role, YamlScalar::Str(primary_sel.clone())));
    }
    if omp_model_supports_images(primary_id) {
        desired_roles.push(("vision", YamlScalar::Str(primary_sel.clone())));
    }
    for role in OMP_FAST_ROLE_KEYS {
        desired_roles.push((*role, YamlScalar::Str(fast_sel.clone())));
    }
    let roles_upsert =
        upsert_yaml_2level_scalars(&mut doc, "modelRoles", &desired_roles).map_err(|detail| {
            HarnessError::UnsupportedYamlLayout {
                path: target_path.to_path_buf(),
                detail,
            }
        })?;

    let approval_mode = if params.no_bypass {
        "always-ask"
    } else {
        "yolo"
    };
    let desired_tools = [
        ("approvalMode", YamlScalar::Str(approval_mode.to_owned())),
        ("commands", YamlScalar::Bool(true)),
    ];
    let tools_upsert =
        upsert_yaml_2level_scalars(&mut doc, "tools", &desired_tools).map_err(|detail| {
            HarnessError::UnsupportedYamlLayout {
                path: target_path.to_path_buf(),
                detail,
            }
        })?;

    let (eval_enabled, eval_ns) = match code_mode {
        OmpCodeModePolicy::Standard => (true, None),
        OmpCodeModePolicy::FullEvalNamespaces => (true, Some(true)),
        OmpCodeModePolicy::Disabled => (false, Some(false)),
    };
    let desired_eval = [
        ("py", YamlScalar::Bool(eval_enabled)),
        ("js", YamlScalar::Bool(eval_enabled)),
    ];
    let eval_upsert =
        upsert_yaml_2level_scalars(&mut doc, "eval", &desired_eval).map_err(|detail| {
            HarnessError::UnsupportedYamlLayout {
                path: target_path.to_path_buf(),
                detail,
            }
        })?;

    let (mcp_was_absent, previous_mcp_header_rhs, previous_mcp_entries, wrote_mcp_entries) =
        if let Some(ns_val) = eval_ns {
            let desired_mcp = [("evalNamespaces", YamlScalar::Bool(ns_val))];
            let mcp_up =
                upsert_yaml_2level_scalars(&mut doc, "mcp", &desired_mcp).map_err(|detail| {
                    HarnessError::UnsupportedYamlLayout {
                        path: target_path.to_path_buf(),
                        detail,
                    }
                })?;
            (
                mcp_up.section_was_absent,
                mcp_up.previous_header_rhs,
                mcp_up.previous_entries,
                mcp_up.wrote_entries,
            )
        } else {
            (false, None, BTreeMap::new(), BTreeMap::new())
        };

    let mut extensions_was_absent = false;
    let mut added_extension_entry = None;
    let mut previous_extensions_raw_lines = None;
    if let Some(ext_items) = explicit_extensions {
        let default_indent = doc.default_indent.max(2);
        if let Some(sec) = doc
            .sections
            .iter_mut()
            .find(|s| s.key.as_deref() == Some("extensions"))
        {
            let (mut current, item_indent) =
                parse_yaml_string_sequence_section(&sec.lines, default_indent).map_err(
                    |detail| HarnessError::UnsupportedYamlLayout {
                        path: target_path.to_path_buf(),
                        detail,
                    },
                )?;
            previous_extensions_raw_lines = Some(sec.lines.clone());
            for item in ext_items {
                if !current.iter().any(|existing| existing == item) {
                    current.push(item.clone());
                    added_extension_entry = Some(item.clone());
                }
            }
            let pad = " ".repeat(item_indent);
            let mut rebuilt = vec!["extensions:\n".to_owned()];
            for item in &current {
                rebuilt.push(format!("{pad}- {}\n", yaml_quote_scalar(item)));
            }
            sec.lines = rebuilt;
        } else if !ext_items.is_empty() {
            extensions_was_absent = true;
            if let Some(last) = doc.sections.last_mut()
                && last.lines.last().is_some_and(|l| !l.trim().is_empty())
            {
                last.lines.push("\n".to_owned());
            }
            let pad = " ".repeat(default_indent);
            let mut lines = vec!["extensions:\n".to_owned()];
            for item in ext_items {
                lines.push(format!("{pad}- {}\n", yaml_quote_scalar(item)));
                added_extension_entry = Some(item.clone());
            }
            doc.sections.push(YamlTopSection {
                key: Some("extensions".to_owned()),
                lines,
            });
        }
    }

    *yaml_buf = render_and_validate_yaml_document(&doc).map_err(|detail| {
        HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        }
    })?;
    Ok(OmpManagedRecord {
        global: explicit_extensions.is_none(),
        config_existed_before_sync: false,
        aux_existed_before_sync: false,
        previous_providers_agy_yaml: None,
        wrote_providers_agy_yaml: None,
        previous_providers_header_rhs: None,
        previous_model_roles: roles_upsert.previous_entries,
        wrote_model_roles: roles_upsert.wrote_entries,
        previous_model_roles_header_rhs: roles_upsert.previous_header_rhs,
        model_roles_was_absent: roles_upsert.section_was_absent,
        added_extension_entry,
        previous_extensions_raw_lines,
        extensions_was_absent,
        previous_tools_entries: tools_upsert.previous_entries,
        wrote_tools_entries: tools_upsert.wrote_entries,
        previous_tools_header_rhs: tools_upsert.previous_header_rhs,
        tools_was_absent: tools_upsert.section_was_absent,
        previous_eval_entries: eval_upsert.previous_entries,
        wrote_eval_entries: eval_upsert.wrote_entries,
        previous_eval_header_rhs: eval_upsert.previous_header_rhs,
        eval_was_absent: eval_upsert.section_was_absent,
        previous_mcp_entries,
        wrote_mcp_entries,
        previous_mcp_header_rhs,
        mcp_was_absent,
    })
}

fn revert_omp_config_yaml_with_record(
    yaml_buf: &mut String,
    rec: &OmpManagedRecord,
    target_path: &Path,
) -> Result<bool, HarnessError> {
    if yaml_buf.trim_start_matches('\u{feff}').trim().is_empty() {
        return Ok(false);
    }
    let mut doc =
        parse_yaml_document(yaml_buf).map_err(|detail| HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        })?;
    let mut changed = false;

    changed |= revert_yaml_2level_scalars(
        &mut doc,
        "modelRoles",
        rec.model_roles_was_absent,
        rec.previous_model_roles_header_rhs.as_deref(),
        &rec.previous_model_roles,
        &rec.wrote_model_roles,
    )
    .map_err(|detail| HarnessError::UnsupportedYamlLayout {
        path: target_path.to_path_buf(),
        detail,
    })?;

    changed |= revert_yaml_2level_scalars(
        &mut doc,
        "tools",
        rec.tools_was_absent,
        rec.previous_tools_header_rhs.as_deref(),
        &rec.previous_tools_entries,
        &rec.wrote_tools_entries,
    )
    .map_err(|detail| HarnessError::UnsupportedYamlLayout {
        path: target_path.to_path_buf(),
        detail,
    })?;

    changed |= revert_yaml_2level_scalars(
        &mut doc,
        "eval",
        rec.eval_was_absent,
        rec.previous_eval_header_rhs.as_deref(),
        &rec.previous_eval_entries,
        &rec.wrote_eval_entries,
    )
    .map_err(|detail| HarnessError::UnsupportedYamlLayout {
        path: target_path.to_path_buf(),
        detail,
    })?;

    changed |= revert_yaml_2level_scalars(
        &mut doc,
        "mcp",
        rec.mcp_was_absent,
        rec.previous_mcp_header_rhs.as_deref(),
        &rec.previous_mcp_entries,
        &rec.wrote_mcp_entries,
    )
    .map_err(|detail| HarnessError::UnsupportedYamlLayout {
        path: target_path.to_path_buf(),
        detail,
    })?;

    if let Some(ref added_ext) = rec.added_extension_entry
        && let Some(idx) = doc
            .sections
            .iter()
            .position(|s| s.key.as_deref() == Some("extensions"))
    {
        let (mut current, item_indent) = {
            let sec = &doc.sections[idx];
            parse_yaml_string_sequence_section(&sec.lines, doc.default_indent).map_err(
                |detail| HarnessError::UnsupportedYamlLayout {
                    path: target_path.to_path_buf(),
                    detail,
                },
            )?
        };
        let before = current.len();
        current.retain(|e| e != added_ext);
        if current.len() != before {
            changed = true;
            if current.is_empty() && rec.extensions_was_absent {
                let _ = doc.sections.remove(idx);
            } else if let Some(ref raw_prev) = rec.previous_extensions_raw_lines
                && parse_yaml_string_sequence_section(raw_prev, doc.default_indent)
                    .is_ok_and(|(prev_items, _)| prev_items == current)
            {
                doc.sections[idx].lines.clone_from(raw_prev);
            } else {
                let pad = " ".repeat(item_indent);
                let mut rebuilt = vec!["extensions:\n".to_owned()];
                for item in &current {
                    rebuilt.push(format!("{pad}- {}\n", yaml_quote_scalar(item)));
                }
                doc.sections[idx].lines = rebuilt;
            }
        }
    }

    if changed {
        *yaml_buf = render_and_validate_yaml_document(&doc).map_err(|detail| {
            HarnessError::UnsupportedYamlLayout {
                path: target_path.to_path_buf(),
                detail,
            }
        })?;
    }
    Ok(changed)
}

fn strip_omp_config_yaml_conservative(
    yaml_buf: &mut String,
    target_path: &Path,
) -> Result<bool, HarnessError> {
    if yaml_buf.trim_start_matches('\u{feff}').trim().is_empty() {
        return Ok(false);
    }
    let mut doc =
        parse_yaml_document(yaml_buf).map_err(|detail| HarnessError::UnsupportedYamlLayout {
            path: target_path.to_path_buf(),
            detail,
        })?;
    let mut changed = false;
    let agy_prefix = format!("{OMP_PROVIDER_ID}/");

    if let Some(idx) = doc
        .sections
        .iter()
        .position(|s| s.key.as_deref() == Some("modelRoles"))
    {
        let sec_lines = doc
            .sections
            .get(idx)
            .map(|s| s.lines.as_slice())
            .unwrap_or_default();
        let parsed =
            parse_mapping_section(sec_lines, doc.default_indent, true).map_err(|detail| {
                HarnessError::UnsupportedYamlLayout {
                    path: target_path.to_path_buf(),
                    detail,
                }
            })?;
        let block_header = parsed.format_block_header();
        let mut kept: Vec<YamlSubEntry> = Vec::new();
        for child in parsed.children {
            let is_agy_role = child
                .lines
                .first()
                .and_then(|l| split_yaml_key_and_rhs(strip_yaml_inline_comment(l).trim()))
                .is_some_and(|(_, rhs)| unquote_yaml_scalar(rhs).starts_with(&agy_prefix));
            if is_agy_role {
                changed = true;
            } else {
                kept.push(child);
            }
        }
        if kept.iter().any(|c| c.key.is_some()) {
            let mut rebuilt = vec![block_header];
            for c in kept {
                rebuilt.extend(c.lines);
            }
            rebuilt.extend(parsed.trailing_blanks);
            if let Some(sec) = doc.sections.get_mut(idx) {
                sec.lines = rebuilt;
            }
        } else if changed {
            let _ = doc.sections.remove(idx);
        }
    }

    if let Some(idx) = doc
        .sections
        .iter()
        .position(|s| s.key.as_deref() == Some("extensions"))
    {
        let (mut current, item_indent) =
            parse_yaml_string_sequence_section(&doc.sections[idx].lines, doc.default_indent)
                .map_err(|detail| HarnessError::UnsupportedYamlLayout {
                    path: target_path.to_path_buf(),
                    detail,
                })?;
        let before = current.len();
        current.retain(|e| e != OMP_LOCAL_EXTENSION_REL_PATH);
        if current.len() != before {
            changed = true;
            if current.is_empty() {
                let _ = doc.sections.remove(idx);
            } else {
                let pad = " ".repeat(item_indent);
                let mut rebuilt = vec!["extensions:\n".to_owned()];
                for item in &current {
                    rebuilt.push(format!("{pad}- {}\n", yaml_quote_scalar(item)));
                }
                doc.sections[idx].lines = rebuilt;
            }
        }
    }

    if changed {
        *yaml_buf = render_and_validate_yaml_document(&doc).map_err(|detail| {
            HarnessError::UnsupportedYamlLayout {
                path: target_path.to_path_buf(),
                detail,
            }
        })?;
    }
    Ok(changed)
}

/// Writes or previews persistent Oh My Pi (`omp`) configuration files without a state ledger.
pub fn write_or_update_omp_config(
    global: bool,
    paths: &OmpResolvedPaths,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, HarnessError> {
    write_or_update_omp_config_with_ledger(None, global, paths, params, print_only)
}

/// Writes or previews persistent Oh My Pi (`omp`) configuration files, recording prior user values
/// in `ledger_path` when provided. Validates all generated YAML before writing any file.
pub fn write_or_update_omp_config_with_ledger(
    ledger_path: Option<&Path>,
    global: bool,
    paths: &OmpResolvedPaths,
    params: &HarnessSyncParams,
    print_only: bool,
) -> Result<String, HarnessError> {
    let (cfg_exists_now, mut cfg_text) = read_optional_utf8_file(&paths.config_path)?;
    let (aux_exists_now, mut aux_text) = read_optional_utf8_file(&paths.auxiliary_path)?;

    let mut ledger = match ledger_path {
        Some(lp) => Some((lp, read_config_sync_ledger(lp)?)),
        None => None,
    };
    let ledger_key = ledger_key_for_path(&paths.config_path);

    let (cfg_existed_before, aux_existed_before) = if let Some((_, ref lg)) = ledger
        && let Some(prior) = lg.omp.get(&ledger_key)
    {
        if cfg_exists_now {
            let _ = revert_omp_config_yaml_with_record(&mut cfg_text, prior, &paths.config_path)?;
        }
        if global && aux_exists_now {
            let (reverted_models, _) = revert_omp_models_yaml(
                &aux_text,
                prior.wrote_providers_agy_yaml.as_deref(),
                prior.previous_providers_agy_yaml.as_deref(),
                prior.previous_providers_header_rhs.as_deref(),
                &paths.auxiliary_path,
            )?;
            aux_text = reverted_models;
        }
        (
            prior.config_existed_before_sync,
            prior.aux_existed_before_sync,
        )
    } else {
        (cfg_exists_now, aux_exists_now)
    };

    let code_mode = if params.no_code_mode {
        OmpCodeModePolicy::Disabled
    } else {
        OmpCodeModePolicy::Standard
    };
    let local_ext_slice = [OMP_LOCAL_EXTENSION_REL_PATH.to_owned()];
    let mut rec = merge_omp_config_yaml_with_record(
        &mut cfg_text,
        params,
        code_mode,
        (!global).then_some(local_ext_slice.as_slice()),
        &paths.config_path,
    )?;
    rec.global = global;
    rec.config_existed_before_sync = cfg_existed_before;
    rec.aux_existed_before_sync = aux_existed_before;

    let aux_rendered = if global {
        let (merged_models, prev_agy, wrote_agy, prev_header_rhs) =
            merge_omp_models_yaml(&aux_text, params, None, &paths.auxiliary_path)?;
        rec.previous_providers_agy_yaml = prev_agy;
        rec.wrote_providers_agy_yaml = Some(wrote_agy);
        rec.previous_providers_header_rhs = prev_header_rhs;
        merged_models
    } else {
        build_omp_provider_extension_ts(params, None)?
    };

    if !print_only {
        backup_once_if_exists(&paths.config_path)?;
        write_file_atomic(&paths.config_path, cfg_text.as_bytes(), true)?;
        if global {
            backup_once_if_exists(&paths.auxiliary_path)?;
        }
        write_file_atomic(&paths.auxiliary_path, aux_rendered.as_bytes(), true)?;
        if let Some((lp, ref mut lg)) = ledger {
            lg.omp.insert(ledger_key, rec);
            write_config_sync_ledger(lp, lg)?;
        }
    }

    Ok(format!(
        "{cfg_text}\n# --- {} ---\n{aux_rendered}",
        paths.auxiliary_path.display()
    ))
}

/// Uninstalls Liftgate's Oh My Pi configuration without a state ledger.
pub fn uninstall_omp_config(global: bool, paths: &OmpResolvedPaths) -> Result<bool, HarnessError> {
    uninstall_omp_config_with_ledger(None, global, paths)
}

/// Uninstalls Liftgate's Oh My Pi configuration, restoring prior displaced values from `ledger_path`.
pub fn uninstall_omp_config_with_ledger(
    ledger_path: Option<&Path>,
    global: bool,
    paths: &OmpResolvedPaths,
) -> Result<bool, HarnessError> {
    let ledger_key = ledger_key_for_path(&paths.config_path);
    let mut any_changed = false;

    if let Some(lp) = ledger_path {
        let mut lg = read_config_sync_ledger(lp)?;
        if let Some(rec) = lg.omp.remove(&ledger_key) {
            let (cfg_exists, mut cfg_text) = read_optional_utf8_file(&paths.config_path)?;
            if cfg_exists
                && revert_omp_config_yaml_with_record(&mut cfg_text, &rec, &paths.config_path)?
            {
                any_changed = true;
                if cfg_text.trim().is_empty() && !rec.config_existed_before_sync {
                    let _ = fs::remove_file(&paths.config_path);
                } else {
                    write_file_atomic(&paths.config_path, cfg_text.as_bytes(), true)?;
                }
            }

            if rec.global {
                let (models_exists, models_text) = read_optional_utf8_file(&paths.auxiliary_path)?;
                if models_exists {
                    let (reverted, models_changed) = revert_omp_models_yaml(
                        &models_text,
                        rec.wrote_providers_agy_yaml.as_deref(),
                        rec.previous_providers_agy_yaml.as_deref(),
                        rec.previous_providers_header_rhs.as_deref(),
                        &paths.auxiliary_path,
                    )?;
                    if models_changed {
                        any_changed = true;
                        if reverted.trim().is_empty() && !rec.aux_existed_before_sync {
                            let _ = fs::remove_file(&paths.auxiliary_path);
                        } else {
                            write_file_atomic(&paths.auxiliary_path, reverted.as_bytes(), true)?;
                        }
                    }
                }
            } else {
                let (ext_exists, ext_text) = read_optional_utf8_file(&paths.auxiliary_path)?;
                if ext_exists
                    && !rec.aux_existed_before_sync
                    && ext_text.starts_with(OMP_MANAGED_EXTENSION_SENTINEL)
                {
                    fs::remove_file(&paths.auxiliary_path).map_err(|source| HarnessError::Io {
                        path: paths.auxiliary_path.clone(),
                        source,
                    })?;
                    any_changed = true;
                }
            }

            if let Some(parent) = paths.config_path.parent() {
                let _ = fs::remove_dir(parent);
            }
            write_config_sync_ledger(lp, &lg)?;
            return Ok(any_changed);
        }
    }

    let (cfg_exists, mut cfg_text) = read_optional_utf8_file(&paths.config_path)?;
    if cfg_exists && strip_omp_config_yaml_conservative(&mut cfg_text, &paths.config_path)? {
        any_changed = true;
        if cfg_text.trim().is_empty() {
            let _ = fs::remove_file(&paths.config_path);
        } else {
            write_file_atomic(&paths.config_path, cfg_text.as_bytes(), true)?;
        }
    }
    if global {
        let (models_exists, models_text) = read_optional_utf8_file(&paths.auxiliary_path)?;
        if models_exists {
            let (reverted, models_changed) =
                revert_omp_models_yaml(&models_text, None, None, None, &paths.auxiliary_path)?;
            if models_changed {
                any_changed = true;
                if reverted.trim().is_empty() {
                    let _ = fs::remove_file(&paths.auxiliary_path);
                } else {
                    write_file_atomic(&paths.auxiliary_path, reverted.as_bytes(), true)?;
                }
            }
        }
    } else {
        let (ext_exists, ext_text) = read_optional_utf8_file(&paths.auxiliary_path)?;
        if ext_exists && ext_text.starts_with(OMP_MANAGED_EXTENSION_SENTINEL) {
            fs::remove_file(&paths.auxiliary_path).map_err(|source| HarnessError::Io {
                path: paths.auxiliary_path.clone(),
                source,
            })?;
            any_changed = true;
        }
    }
    if let Some(parent) = paths.config_path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(any_changed)
}

/// Inspects `modelRoles.default` across 2-space, 4-space, or inline flow mappings.
#[must_use]
pub fn inspect_omp_default_model(cfg_text: &str) -> Option<String> {
    let doc = parse_yaml_document(cfg_text).ok()?;
    let roles_sec = doc
        .sections
        .iter()
        .find(|s| s.key.as_deref() == Some("modelRoles"))?;
    let parsed = parse_mapping_section(&roles_sec.lines, doc.default_indent, true).ok()?;
    let default_entry = parsed
        .children
        .iter()
        .find(|c| c.key.as_deref() == Some("default"))?;
    let first = default_entry.lines.first()?;
    let (_, rhs) = split_yaml_key_and_rhs(strip_yaml_inline_comment(first).trim())?;
    let val = unquote_yaml_scalar(rhs);
    (!val.is_empty()).then_some(val)
}

/// Returns `true` when `models.yml` declares a `providers.agy` block at any valid indentation.
#[must_use]
pub fn has_omp_agy_provider_yaml(models_text: &str) -> bool {
    let Ok(doc) = parse_yaml_document(models_text) else {
        return false;
    };
    let Some(sec) = doc
        .sections
        .iter()
        .find(|s| s.key.as_deref() == Some("providers"))
    else {
        return false;
    };
    let Ok(parsed) = parse_mapping_section(&sec.lines, doc.default_indent, false) else {
        return false;
    };
    parsed
        .children
        .iter()
        .any(|c| c.key.as_deref() == Some(OMP_PROVIDER_ID))
}
