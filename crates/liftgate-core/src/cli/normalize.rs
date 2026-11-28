//! Pre-Clap argv normalizer separating Liftgate flags from passthrough harness arguments.

use std::ffi::OsString;

use liftgate_harness::{
    claude::rewrite_claude_session_aliases, codex::rewrite_codex_session_aliases,
    omp::rewrite_omp_session_aliases, opencode::rewrite_opencode_session_aliases,
};

/// Normalizes `liftgate claude ...` and `liftgate codex ...` argv sequences so callers can pass
/// native `claude` / `codex` flags or prompts directly (e.g. `liftgate claude -p "hi"` or
/// `liftgate codex exec "fix tests"`) without needing an explicit `--` separator.
#[must_use]
pub fn normalize_harness_args(raw_args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let args: Vec<OsString> = raw_args.into_iter().collect();
    let Some(sub_idx) = find_subcommand_index(&args) else {
        return args;
    };
    let Some(sub_name) = args.get(sub_idx).and_then(|s| s.to_str()) else {
        return args;
    };
    let Some(kind) = HarnessSubcommand::from_name(sub_name) else {
        return args;
    };
    let Some(rest) = args.get(sub_idx + 1..) else {
        return args;
    };
    if rest.is_empty() {
        return args;
    }

    let dd_pos = rest.iter().position(|t| t == "--");
    let pre_dd = match dd_pos {
        Some(pos) => rest.get(..pos).unwrap_or_default(),
        None => rest,
    };
    let post_dd = dd_pos.and_then(|pos| rest.get(pos..));

    let is_desktop_mode = detect_desktop_mode(kind, pre_dd);
    let value_flags = harness_value_flags(kind);
    let bool_flags = harness_bool_flags(kind, is_desktop_mode);

    // Caller explicitly separated Liftgate options from raw child arguments (`lg cx [-m ...] -- ...`).
    if dd_pos.is_some() && all_tokens_are_liftgate_flags(pre_dd, value_flags, bool_flags) {
        return args;
    }

    let native_value_flags = native_single_value_flags(kind);
    let mut lg_args = Vec::new();
    let mut child_args = Vec::new();
    let mut help_positions: Vec<(usize, OsString)> = Vec::new();

    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let Some(s) = tok.to_str() else {
            child_args.push(tok.clone());
            idx += 1;
            continue;
        };
        if matches!(s, "-h" | "--help") {
            help_positions.push((child_args.len(), tok.clone()));
            idx += 1;
            continue;
        }
        if bool_flags.contains(&s) {
            lg_args.push(tok.clone());
            idx += 1;
            continue;
        }
        if value_flags.contains(&s) {
            lg_args.push(tok.clone());
            if let Some(val) = pre_dd.get(idx + 1) {
                lg_args.push(val.clone());
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        if value_flags
            .iter()
            .any(|vf| s.strip_prefix(vf).is_some_and(|rest| rest.starts_with('=')))
        {
            lg_args.push(tok.clone());
            idx += 1;
            continue;
        }
        if native_value_flags.contains(&s) {
            child_args.push(tok.clone());
            if let Some(val) = pre_dd.get(idx + 1) {
                child_args.push(val.clone());
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        child_args.push(tok.clone());
        idx += 1;
    }

    if let Some(tail) = post_dd {
        child_args.extend_from_slice(tail);
    }

    if child_args.is_empty() {
        for (_, flag) in help_positions {
            lg_args.push(flag);
        }
    } else {
        for (offset, (pos, flag)) in help_positions.into_iter().enumerate() {
            let insert_at = (pos + offset).min(child_args.len());
            child_args.insert(insert_at, flag);
        }
    }

    if !is_desktop_mode {
        match kind {
            HarnessSubcommand::Claude => rewrite_claude_session_aliases(&mut child_args),
            HarnessSubcommand::Codex => rewrite_codex_session_aliases(&mut child_args),
            HarnessSubcommand::Opencode => rewrite_opencode_session_aliases(&mut child_args),
            HarnessSubcommand::Omp => rewrite_omp_session_aliases(&mut child_args),
            HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => {}
        }
    }

    let mut out = Vec::with_capacity(sub_idx + 2 + lg_args.len() + child_args.len());
    if let Some(prefix) = args.get(..=sub_idx) {
        out.extend_from_slice(prefix);
    }
    out.extend(lg_args);
    if !child_args.is_empty() {
        out.push(OsString::from("--"));
        out.extend(child_args);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessSubcommand {
    Claude,
    ClaudeDesktop,
    Codex,
    CodexApp,
    Opencode,
    Omp,
}

impl HarnessSubcommand {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" | "cc" => Some(Self::Claude),
            "claude-desktop" | "cd" | "claude-app" => Some(Self::ClaudeDesktop),
            "codex" | "cx" | "x" => Some(Self::Codex),
            "codex-app" | "cxa" | "codex-desktop" => Some(Self::CodexApp),
            "opencode" | "oc" => Some(Self::Opencode),
            "omp" | "oh-my-pi" => Some(Self::Omp),
            _ => None,
        }
    }
}

fn detect_desktop_mode(kind: HarnessSubcommand, pre_dd: &[OsString]) -> bool {
    match kind {
        HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => true,
        HarnessSubcommand::Opencode | HarnessSubcommand::Omp => false,
        HarnessSubcommand::Codex => pre_dd
            .iter()
            .any(|t| matches!(t.to_str(), Some("--app" | "--desktop"))),
        HarnessSubcommand::Claude => {
            let has_app = pre_dd.iter().any(|t| t.to_str() == Some("--app"));
            let has_desktop = pre_dd.iter().any(|t| t.to_str() == Some("--desktop"));
            let has_lg_desktop_only = pre_dd.iter().any(|t| {
                t.to_str().is_some_and(|s| {
                    matches!(s, "--restart-app" | "--wait" | "--claude-desktop-bin")
                        || s.starts_with("--claude-desktop-bin=")
                })
            });
            let has_session_handoff = pre_dd.iter().any(|t| {
                t.to_str().is_some_and(|s| {
                    matches!(
                        s,
                        "-c" | "--continue" | "-r" | "--resume" | "resume" | "continue"
                    ) || s.starts_with("--resume=")
                })
            });
            has_app || (has_desktop && (has_lg_desktop_only || !has_session_handoff))
        }
    }
}

fn harness_value_flags(kind: HarnessSubcommand) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Claude => &[
            "--state-dir",
            "--listen",
            "--remote",
            "--api-key",
            "-m",
            "--model",
            "--fast-model",
            "--claude-bin",
            "--claude-desktop-bin",
        ],
        HarnessSubcommand::ClaudeDesktop => &[
            "--state-dir",
            "--listen",
            "--remote",
            "--api-key",
            "-m",
            "--model",
            "--claude-desktop-bin",
        ],
        HarnessSubcommand::Codex | HarnessSubcommand::CodexApp => &[
            "--state-dir",
            "--listen",
            "--remote",
            "--api-key",
            "-m",
            "--model",
            "--codex-bin",
            "--codex-app-bin",
        ],
        HarnessSubcommand::Opencode => &[
            "--state-dir",
            "--listen",
            "--remote",
            "--api-key",
            "-m",
            "--model",
            "--fast-model",
            "--opencode-bin",
        ],
        HarnessSubcommand::Omp => &[
            "--state-dir",
            "--listen",
            "--remote",
            "--api-key",
            "-m",
            "--model",
            "--fast-model",
            "--omp-bin",
        ],
    }
}

fn harness_bool_flags(kind: HarnessSubcommand, is_desktop_mode: bool) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Codex => {
            if is_desktop_mode {
                &[
                    "--no-bypass",
                    "--code-mode",
                    "--code-mode-only",
                    "--no-code-mode",
                    "--app",
                    "--desktop",
                    "--restart-app",
                    "--ephemeral",
                    "-w",
                    "--wait",
                ]
            } else {
                &[
                    "--no-bypass",
                    "--code-mode",
                    "--code-mode-only",
                    "--no-code-mode",
                ]
            }
        }
        HarnessSubcommand::CodexApp => &[
            "--no-bypass",
            "--code-mode",
            "--code-mode-only",
            "--no-code-mode",
            "--restart-app",
            "--ephemeral",
            "-w",
            "--wait",
        ],
        HarnessSubcommand::Claude => {
            if is_desktop_mode {
                &[
                    "--no-bypass",
                    "--no-code-mode",
                    "--desktop",
                    "--app",
                    "--restart-app",
                    "-w",
                    "--wait",
                ]
            } else {
                &["--no-bypass", "--no-code-mode"]
            }
        }
        HarnessSubcommand::ClaudeDesktop => &["--restart-app", "--restore", "-w", "--wait"],
        HarnessSubcommand::Opencode => &[
            "--no-bypass",
            "--full-code-mode",
            "--code-mode-only",
            "--mcp-code-mode",
            "--no-code-mode",
        ],
        HarnessSubcommand::Omp => &[
            "--no-bypass",
            "--full-code-mode",
            "--code-mode-only",
            "--no-code-mode",
        ],
    }
}

fn native_single_value_flags(kind: HarnessSubcommand) -> &'static [&'static str] {
    match kind {
        HarnessSubcommand::Codex => &[
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
            "-o",
            "--output-last-message",
            "--output-schema",
            "--thread",
            "--message",
            "--base",
            "--commit",
            "--title",
            "--thread-source",
            "--cyber-access-program",
        ],
        HarnessSubcommand::Claude => &[
            "--agent",
            "--agents",
            "--append-system-prompt",
            "--autocompact",
            "--betas",
            "--debug-file",
            "--effort",
            "--environment",
            "--fallback-model",
            "--input-format",
            "--json-schema",
            "--max-budget-usd",
            "--name",
            "-n",
            "--output-format",
            "--permission-mode",
            "--permission-prompts",
            "--plugin-dir",
            "--plugin-url",
            "--remote-control-session-name-prefix",
            "--session-id",
            "--setting-sources",
            "--settings",
            "--system-prompt",
            "--system-prompt-snapshot",
        ],
        HarnessSubcommand::Opencode => &[
            "-s",
            "--session",
            "--prompt",
            "--server",
            "--url",
            "--agent",
            "--format",
            "-f",
            "--file",
            "--title",
            "--directory",
            "--method",
            "--answer",
            "--header",
            "--env",
            "-n",
            "--max-count",
        ],
        HarnessSubcommand::Omp => &[
            "--cwd",
            "--add-dir",
            "--profile",
            "--alias",
            "--provider",
            "--config",
            "--smol",
            "--slow",
            "--plan",
            "--goal",
            "--prewalk-into",
            "--plan-yolo-into",
            "--max-time",
            "--warn-cost",
            "--max-cost",
            "--system-prompt",
            "--system-prompt-template",
            "--append-system-prompt",
            "--thinking",
            "--service-tier",
            "--mode",
            "--session-dir",
            "--provider-session-id",
            "--provider-prompt-cache-key",
            "--fork",
            "--models",
            "--tools",
            "--hook",
            "-e",
            "--extension",
            "--trusted-extension",
            "--plugin-dir",
            "--export",
            "--skills",
            "--spinner-interval",
            "--max-fps",
            "--motion",
            "--approval-mode",
            "--kind",
        ],
        HarnessSubcommand::ClaudeDesktop | HarnessSubcommand::CodexApp => &[],
    }
}

fn all_tokens_are_liftgate_flags(
    tokens: &[OsString],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> bool {
    let mut idx = 0usize;
    while let Some(tok) = tokens.get(idx) {
        let Some(s) = tok.to_str() else {
            return false;
        };
        if matches!(s, "-h" | "--help") || bool_flags.contains(&s) {
            idx += 1;
            continue;
        }
        if value_flags.contains(&s) {
            if tokens.get(idx + 1).is_none() {
                return false;
            }
            idx += 2;
            continue;
        }
        if value_flags
            .iter()
            .any(|vf| s.strip_prefix(vf).is_some_and(|rest| rest.starts_with('=')))
        {
            idx += 1;
            continue;
        }
        return false;
    }
    true
}

fn find_subcommand_index(args: &[OsString]) -> Option<usize> {
    const TOP_VALUE_FLAGS: &[&str] = &[
        "--state-dir",
        "--listen",
        "--ui-listen",
        "--agy-bin",
        "--default-model",
        "--acp-mode",
        "--acp-cwd",
        "--api-key",
        "--ui-dir",
        "--agy-proxy",
        "--agy-auto-update",
        "--lg-auto-update",
        "--vision-offload",
        "--mcp-tool-bridge",
        "--low-cache-capture-payload",
        "--max-low-cache-records",
        "--low-cache-threshold-pct",
        "--max-line-bytes",
        "--max-body-bytes",
        "--channel-capacity",
        "--max-idle-sessions",
        "--max-stored-responses",
        "--turn-timeout-secs",
    ];

    let mut idx = 1usize;
    while let Some(tok) = args.get(idx) {
        let s = tok.to_str()?;
        if s == "--" {
            return None;
        }
        if !s.starts_with('-') {
            return Some(idx);
        }
        if TOP_VALUE_FLAGS.contains(&s) {
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}
