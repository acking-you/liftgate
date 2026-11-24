//! Oh My Pi (`omp`) CLI invocation classification, `PI_CONFIG_FILES` merging, and alias rewriting.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::omp::config::{extract_omp_yaml_extensions, resolve_omp_paths};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpInvocationKind {
    /// Local/credential/plugin/stats/update/help/version invocation executed directly without
    /// starting the gateway or injecting `PI_CONFIG_FILES`.
    DirectPassthrough,
    /// Interactive TUI (`omp`, `-c`, `-r`, `--fork`), `-p`, `models`, `commit`, `session`,
    /// `acp`, or `stream` routed through Liftgate via ephemeral `PI_CONFIG_FILES`.
    GatewaySession,
}

#[must_use]
pub fn classify_omp_invocation(passthrough_args: &[String]) -> OmpInvocationKind {
    if passthrough_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help" | "-v" | "--version" | "--license"
        )
    }) {
        return OmpInvocationKind::DirectPassthrough;
    }
    let first_non_flag = passthrough_args.first().filter(|a| !a.starts_with('-'));
    match first_non_flag.map(String::as_str) {
        Some(
            "help" | "auth" | "auth-broker" | "auth-gateway" | "login" | "plugin" | "plugins"
            | "install" | "update" | "completions" | "__complete" | "stats" | "toks" | "token"
            | "usage" | "config" | "gc" | "cleanse" | "anonymize" | "ps" | "ssh" | "worktree"
            | "wt" | "skill" | "skills" | "tiny-models",
        ) => OmpInvocationKind::DirectPassthrough,
        Some(_) | None => OmpInvocationKind::GatewaySession,
    }
}

#[must_use]
pub fn collect_existing_omp_extensions(cwd: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(global_paths) = resolve_omp_paths(true, cwd)
        && let Ok(text) = std::fs::read_to_string(&global_paths.config_path)
    {
        out.extend(extract_omp_yaml_extensions(&text));
    }
    if let Ok(local_paths) = resolve_omp_paths(false, cwd)
        && let Ok(text) = std::fs::read_to_string(&local_paths.config_path)
    {
        for item in extract_omp_yaml_extensions(&text) {
            if !out.iter().any(|existing| existing == &item) {
                out.push(item);
            }
        }
    }
    out
}

#[must_use]
pub fn merge_omp_config_files_env(existing: Option<&OsStr>, overlay_path: &Path) -> String {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let mut parts: Vec<String> = existing
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    parts.push(overlay_path.display().to_string());
    parts.join(&sep.to_string())
}

pub fn rewrite_omp_session_aliases(child_args: &mut Vec<OsString>) {
    let is_alias = matches!(
        child_args.first().and_then(|s| s.to_str()),
        Some("resume" | "continue")
    );
    if !is_alias {
        return;
    }
    let tail = child_args.split_off(1);
    let first_owned = child_args.pop().unwrap_or_default();
    let Some(first) = first_owned.to_str() else {
        return;
    };
    let second = tail.first().and_then(|s| s.to_str());
    match first {
        "continue" => {
            if !matches!(second, Some("-h" | "--help")) {
                child_args.push(OsString::from("--continue"));
            }
            child_args.extend(tail);
        }
        "resume" => match second {
            Some("-h" | "--help") => {
                child_args.extend(tail);
            }
            None => {
                child_args.push(OsString::from("--resume"));
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            Some(target) if !target.starts_with('-') => {
                child_args.push(OsString::from("--resume"));
                child_args.push(OsString::from(target));
                if let Some(after_target) = tail.get(1..) {
                    child_args.extend_from_slice(after_target);
                }
            }
            _ => {
                child_args.push(OsString::from("--resume"));
                child_args.extend(tail);
            }
        },
        _ => {}
    }
}
