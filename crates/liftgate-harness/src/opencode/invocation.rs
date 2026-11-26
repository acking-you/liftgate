//! OpenCode CLI invocation classification, feature flag helpers, and session alias rewriting.

use std::ffi::OsString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeInvocationKind {
    /// Local/credential/plugin/MCP/upgrade/help/version invocation executed directly without
    /// starting the gateway or injecting `OPENCODE_CONFIG_CONTENT`.
    DirectPassthrough,
    /// Interactive TUI (`opencode`, `-c`, `-s <ID>`), `run`, `models`, `session`, or server
    /// subcommands (`web`, `serve`, `acp`, `attach`) routed through Liftgate.
    GatewaySession {
        /// Whether OpenCode v2 (`>= 2.0.0`) accepts `--standalone` on this command so a private
        /// server reads `OPENCODE_CONFIG_CONTENT` instead of attaching to an older background daemon.
        supports_v2_standalone: bool,
    },
}

#[must_use]
pub fn classify_opencode_invocation(passthrough_args: &[String]) -> OpenCodeInvocationKind {
    if passthrough_args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "-v" | "--version"))
    {
        return OpenCodeInvocationKind::DirectPassthrough;
    }
    let first_non_flag = passthrough_args.first().filter(|a| !a.starts_with('-'));
    match first_non_flag.map(String::as_str) {
        Some(
            "upgrade" | "update" | "uninstall" | "auth" | "login" | "logout" | "mcp" | "plugin"
            | "completion" | "stats" | "export" | "import" | "debug" | "service",
        ) => OpenCodeInvocationKind::DirectPassthrough,
        Some("web" | "serve" | "acp" | "attach") => OpenCodeInvocationKind::GatewaySession {
            supports_v2_standalone: false,
        },
        Some("run" | "models" | "session") | Some(_) | None => {
            OpenCodeInvocationKind::GatewaySession {
                supports_v2_standalone: true,
            }
        }
    }
}

pub fn parse_opencode_major_version(text: &str) -> Option<u32> {
    for token in text.split_whitespace() {
        let trimmed = token.trim_start_matches('v');
        if let Some((major_str, rest)) = trimmed.split_once('.')
            && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            && let Ok(major) = major_str.parse::<u32>()
        {
            return Some(major);
        }
    }
    None
}

/// Returns `true` when `existing` `OPENCODE_FLAGS` explicitly contains `-{flag}`.
#[must_use]
pub fn has_negated_opencode_flag(existing: Option<&str>, flag: &str) -> bool {
    let neg = format!("-{flag}");
    existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .any(|s| s == neg)
}

/// Enables `flag` in `OPENCODE_FLAGS`, stripping any negated `-{flag}` entry.
#[must_use]
pub fn merge_opencode_flag(existing: Option<&str>, flag: &str) -> String {
    let neg = format!("-{flag}");
    let mut items: Vec<&str> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != neg)
        .collect();
    if !items.contains(&flag) {
        items.push(flag);
    }
    items.join(",")
}

/// Disables `flag` in `OPENCODE_FLAGS` by removing `flag` and appending `-{flag}`.
#[must_use]
pub fn strip_opencode_flag(existing: Option<&str>, flag: &str) -> String {
    let neg = format!("-{flag}");
    let mut items: Vec<&str> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != flag)
        .collect();
    if !items.iter().any(|s| *s == neg) {
        items.push(&neg);
    }
    items.join(",")
}

pub fn rewrite_opencode_session_aliases(child_args: &mut Vec<OsString>) {
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
                child_args.push(OsString::from("--continue"));
            }
            Some("--last") => {
                child_args.push(OsString::from("--continue"));
                if let Some(after_last) = tail.get(1..) {
                    child_args.extend_from_slice(after_last);
                }
            }
            Some(target) if !target.starts_with('-') => {
                child_args.push(OsString::from("--session"));
                child_args.push(OsString::from(target));
                if let Some(after_target) = tail.get(1..) {
                    child_args.extend_from_slice(after_target);
                }
            }
            _ => {
                child_args.push(OsString::from("--continue"));
                child_args.extend(tail);
            }
        },
        _ => {}
    }
}
