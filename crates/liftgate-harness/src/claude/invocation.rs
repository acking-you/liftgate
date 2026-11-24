//! Claude Code CLI invocation classification, environment builders, and session alias rewriting.

use std::ffi::OsString;

use liftgate_types::config::DEFAULT_TURN_TIMEOUT_SECS;

use crate::args_util::slice_before_double_dash;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = DEFAULT_TURN_TIMEOUT_SECS * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeInvocationKind {
    /// Local utility, background job control, help, or version invocation that must run with the
    /// native subcommand in `argv[1]` and without starting the gateway.
    DirectPassthrough,
    /// Subcommand requiring `argv[1]` intact (`respawn`, `doctor`, `ultrareview`) while inheriting
    /// Liftgate's `ANTHROPIC_*` environment variables.
    GatewayEnvSubcommand,
    /// Interactive `claude agents` TUI: keeps `agents` in `argv[1]` while passing Liftgate's
    /// ephemeral `--settings`, `--model`, permissions, and `ANTHROPIC_*` env for dispatched sessions.
    AgentsView,
    /// Interactive, `--print`, `--resume`, or `--continue` coding session.
    AgentSession,
}

#[must_use]
pub fn classify_claude_invocation(passthrough_args: &[String]) -> ClaudeInvocationKind {
    let pre_dd = slice_before_double_dash(passthrough_args);
    if pre_dd.iter().any(|a| matches!(a.as_str(), "-h" | "--help")) {
        return ClaudeInvocationKind::DirectPassthrough;
    }

    if let Some(first) = pre_dd.first().map(String::as_str) {
        match first {
            "agents" => {
                if pre_dd.iter().any(|a| a == "--json") {
                    return ClaudeInvocationKind::DirectPassthrough;
                }
                return ClaudeInvocationKind::AgentsView;
            }
            "help" | "-v" | "--version" | "attach" | "logs" | "stop" | "kill" | "rm" | "mcp"
            | "plugin" | "plugins" | "auth" | "auto-mode" | "import" | "install" | "purge"
            | "setup-token" | "update" | "upgrade" | "gateway" => {
                return ClaudeInvocationKind::DirectPassthrough;
            }
            "respawn" | "doctor" | "ultrareview" => {
                return ClaudeInvocationKind::GatewayEnvSubcommand;
            }
            _ => {}
        }
    }

    if pre_dd
        .iter()
        .any(|a| matches!(a.as_str(), "-v" | "--version"))
    {
        return ClaudeInvocationKind::DirectPassthrough;
    }

    ClaudeInvocationKind::AgentSession
}

#[must_use]
pub fn default_claude_effort_level(model_id: &str) -> Option<&'static str> {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.ends_with("-xhigh") {
        Some("xhigh")
    } else if lower.ends_with("-high") || lower.ends_with("-thinking") {
        Some("high")
    } else if lower.ends_with("-medium") {
        Some("medium")
    } else if lower.ends_with("-low") {
        Some("low")
    } else {
        None
    }
}

#[must_use]
pub fn build_claude_gateway_env_pairs(
    base_url: &str,
    auth_token: &str,
    primary_model: &str,
    fast_model: &str,
    no_code_mode: bool,
    no_proxy: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_owned(), base_url.to_owned()),
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), auth_token.to_owned()),
        ("ANTHROPIC_MODEL".to_owned(), primary_model.to_owned()),
        (
            "ANTHROPIC_DEFAULT_FABLE_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_owned(),
            primary_model.to_owned(),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_owned(),
            fast_model.to_owned(),
        ),
        (
            "ANTHROPIC_SMALL_FAST_MODEL".to_owned(),
            fast_model.to_owned(),
        ),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
            "1".to_owned(),
        ),
        ("CLAUDE_CODE_AUTO_MODE_SERVER".to_owned(), "0".to_owned()),
        (
            "CLAUDE_CODE_EXPERIMENTAL_CODE_MODE".to_owned(),
            if no_code_mode { "0" } else { "1" }.to_owned(),
        ),
        ("NO_PROXY".to_owned(), no_proxy.to_owned()),
        ("no_proxy".to_owned(), no_proxy.to_owned()),
    ];
    for key in ["API_TIMEOUT_MS", "CLAUDE_STREAM_IDLE_TIMEOUT_MS"] {
        if std::env::var_os(key).is_none() {
            env_set.push((key.to_owned(), DEFAULT_REQUEST_TIMEOUT_MS.to_string()));
        }
    }
    let env_remove = vec![
        "ANTHROPIC_API_KEY".to_owned(),
        "AGY_API_KEY".to_owned(),
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_owned(),
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_owned(),
    ];
    (env_set, env_remove)
}

pub fn rewrite_claude_session_aliases(child_args: &mut Vec<OsString>) {
    let is_alias = matches!(
        child_args.first().and_then(|s| s.to_str()),
        Some("resume" | "continue" | "fork")
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
        "resume" => match second {
            Some("-h" | "--help") => {
                child_args.extend(tail);
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            _ => {
                child_args.push(OsString::from("--resume"));
                child_args.extend(tail);
            }
        },
        "continue" => {
            if !matches!(second, Some("-h" | "--help")) {
                child_args.push(OsString::from("--continue"));
            }
            child_args.extend(tail);
        }
        "fork" => match second {
            Some("-h" | "--help") => {
                child_args.extend(tail);
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                child_args.push(OsString::from("--fork-session"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            Some(target) if !target.starts_with('-') => {
                child_args.push(OsString::from("--resume"));
                if let Some(first_tail) = tail.first() {
                    child_args.push(first_tail.clone());
                }
                child_args.push(OsString::from("--fork-session"));
                if let Some(after_target) = tail.get(1..) {
                    child_args.extend_from_slice(after_target);
                }
            }
            _ => {
                child_args.push(OsString::from("--resume"));
                child_args.push(OsString::from("--fork-session"));
                child_args.extend(tail);
            }
        },
        _ => {}
    }
}
