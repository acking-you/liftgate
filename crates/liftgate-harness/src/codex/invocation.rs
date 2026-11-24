//! Codex CLI invocation classification, TOML inline escaping, and session alias rewriting.

use std::ffi::OsString;

use crate::args_util::slice_before_double_dash;

pub const CODEX_STREAM_IDLE_TIMEOUT_KEY: &str = "model_providers.agy.stream_idle_timeout_ms";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexInvocationKind {
    /// Local/session-store/IPC/help/version invocation (`queue`, `archive`, `delete`, `mcp`,
    /// `plugin`, `features`, `completion`, `sandbox`, `--help`, `-V`, etc.) executed directly.
    DirectPassthrough,
    /// Run the native updater with the caller's environment unchanged.
    NativeUpdate,
    /// Gateway-backed subcommand (`review`, `doctor`, `debug`, `app-server`, etc.)
    /// configured solely via `-c` overrides without `--dangerously-bypass-approvals-and-sandbox`.
    GatewayConfigOnly,
    /// Interactive agents overview, with the same config flags as other gateway subcommands.
    AgentsView,
    /// Start, resume, or fork an interactive session.
    InteractiveSession,
    /// Run an agent session non-interactively.
    ExecSession,
}

#[must_use]
pub fn classify_codex_invocation(passthrough_args: &[String]) -> CodexInvocationKind {
    let pre_dd = slice_before_double_dash(passthrough_args);
    let subcommand = find_codex_root_subcommand(pre_dd);
    if matches!(subcommand, Some(("update", _))) {
        return CodexInvocationKind::NativeUpdate;
    }
    if pre_dd
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return CodexInvocationKind::DirectPassthrough;
    }

    let Some((sub, sub_idx)) = subcommand else {
        return CodexInvocationKind::InteractiveSession;
    };

    match sub {
        "help" | "completion" | "queue" | "archive" | "unarchive" | "delete" | "mcp" | "plugin"
        | "features" | "login" | "logout" | "apply" | "a" | "cloud" | "sandbox"
        | "migrate-rollouts" | "migrate-nfs-db" | "app" => CodexInvocationKind::DirectPassthrough,
        "app-server" => {
            if matches!(
                pre_dd.get(sub_idx + 1).map(String::as_str),
                Some("daemon" | "proxy" | "generate-ts" | "generate-json-schema" | "help")
            ) {
                CodexInvocationKind::DirectPassthrough
            } else {
                CodexInvocationKind::GatewayConfigOnly
            }
        }
        "agents" => CodexInvocationKind::AgentsView,
        "review" | "doctor" | "debug" | "remote-control" | "exec-server" => {
            CodexInvocationKind::GatewayConfigOnly
        }
        "exec" | "e" => {
            if pre_dd.get(sub_idx + 1).map(String::as_str) == Some("help") {
                CodexInvocationKind::DirectPassthrough
            } else {
                CodexInvocationKind::ExecSession
            }
        }
        _ => CodexInvocationKind::InteractiveSession,
    }
}

fn find_codex_root_subcommand(pre_dd: &[String]) -> Option<(&str, usize)> {
    const ROOT_VALUE_OPTS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote",
        "--remote-auth-token-env",
        "-i",
        "--image",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];
    const KNOWN_SUBCOMMANDS: &[&str] = &[
        "agents",
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "resume",
        "queue",
        "archive",
        "delete",
        "migrate-rollouts",
        "migrate-nfs-db",
        "unarchive",
        "fork",
        "cloud",
        "exec-server",
        "features",
        "help",
    ];

    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        if !s.starts_with('-') {
            return KNOWN_SUBCOMMANDS.contains(&s).then_some((s, idx));
        }
        if ROOT_VALUE_OPTS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

#[must_use]
pub fn has_explicit_codex_config_key(args: &[String], key: &str) -> bool {
    let pre_dd = slice_before_double_dash(args);
    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        let expr = if matches!(s, "-c" | "--config") {
            idx += 2;
            pre_dd.get(idx - 1).map(String::as_str)
        } else if let Some(rest) = s
            .strip_prefix("--config=")
            .or_else(|| s.strip_prefix("-c="))
        {
            idx += 1;
            Some(rest)
        } else {
            idx += 1;
            None
        };
        if let Some(kv) = expr
            && let Some((lhs, _)) = kv.split_once('=')
            && lhs.trim() == key
        {
            return true;
        }
    }
    false
}

#[must_use]
pub fn toml_inline_string(val: &str) -> String {
    if !val.contains('\'') && !val.chars().any(char::is_control) {
        format!("'{val}'")
    } else {
        serde_json::to_string(val).unwrap_or_else(|_| format!("{val:?}"))
    }
}

pub fn rewrite_codex_session_aliases(child_args: &mut Vec<OsString>) {
    const ROOT_VALUE_OPTS: &[&str] = &[
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote",
        "--remote-auth-token-env",
        "-i",
        "--image",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];

    let mut idx = 0usize;
    while let Some(tok) = child_args.get(idx) {
        let Some(s) = tok.to_str() else {
            return;
        };
        if s == "--" {
            return;
        }
        if matches!(s, "-r" | "--resume") {
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            return;
        }
        if let Some(target) = s
            .strip_prefix("--resume=")
            .or_else(|| s.strip_prefix("-r="))
        {
            let target_owned = target.to_owned();
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            if !target_owned.is_empty() {
                child_args.insert(idx + 1, OsString::from(target_owned));
            }
            return;
        }
        if s == "--continue" {
            if let Some(slot) = child_args.get_mut(idx) {
                *slot = OsString::from("resume");
            }
            child_args.insert(idx + 1, OsString::from("--last"));
            return;
        }
        if !s.starts_with('-') {
            return;
        }
        if ROOT_VALUE_OPTS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
}
