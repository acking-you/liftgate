use super::*;

fn parse_strs(items: &[&str]) -> Cli {
    let normalized = normalize_harness_args(items.iter().map(OsString::from));
    Cli::try_parse_from(normalized).expect("CLI arguments should parse")
}

#[test]
fn bare_invocation_defaults_to_foreground_serve_compat_mode() {
    let cli = parse_strs(&[
        "liftgate",
        "--listen",
        "127.0.0.1:9123",
        "--default-model",
        "gemini-3.8-pro-high",
    ]);
    assert!(cli.command.is_none());
    assert_eq!(cli.serve.listen.port(), 9123);
    assert_eq!(cli.serve.default_model, "gemini-3.8-pro-high");
    assert!(!cli.serve.no_vision_offload);
    let start = parse_strs(&["lg", "start", "--force", "--no-vision-offload"]);
    assert!(matches!(
        start.command,
        Some(Commands::Start(s)) if s.force && s.serve.no_vision_offload
    ));
    let restart = parse_strs(&[
        "lg",
        "restart",
        "--no-agy-auto-update",
        "--vision-offload",
        "false",
    ]);
    assert!(matches!(
        restart.command,
        Some(Commands::Restart(s)) if s.no_agy_auto_update && s.vision_offload == Some(false)
    ));
}

#[test]
fn claude_and_codex_forward_native_flags_without_requiring_double_dash() {
    let cli = parse_strs(&[
        "liftgate",
        "claude",
        "-m",
        "gemini-3.8-pro-high",
        "-p",
        "summarize repo",
    ]);
    match cli.command {
        Some(Commands::Claude(c)) => {
            assert_eq!(c.model.as_deref(), Some("gemini-3.8-pro-high"));
            assert_eq!(c.passthrough_args, vec!["-p", "summarize repo"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let codex_cli = parse_strs(&["liftgate", "codex", "exec", "cargo check"]);
    match codex_cli.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(c.passthrough_args, vec!["exec", "cargo check"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let short_claude = parse_strs(&["lg", "cc", "--no-code-mode", "-p", "quick check"]);
    match short_claude.command {
        Some(Commands::Claude(c)) => {
            assert!(c.no_code_mode);
            assert_eq!(c.passthrough_args, vec!["-p", "quick check"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let short_codex = parse_strs(&["lg", "cx", "exec", "cargo test"]);
    match short_codex.command {
        Some(Commands::Codex(c)) => {
            assert!(!c.code_mode && !c.code_mode_only && !c.no_code_mode);
            assert_eq!(c.passthrough_args, vec!["exec", "cargo test"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let top_flag_codex = parse_strs(&[
        "lg",
        "--mcp-tool-bridge",
        "false",
        "--low-cache-capture-payload",
        "false",
        "cx",
        "-p",
        "quick verify",
    ]);
    assert_eq!(top_flag_codex.serve.mcp_tool_bridge, Some(false));
    assert_eq!(top_flag_codex.serve.low_cache_capture_payload, Some(false));
    match top_flag_codex.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(c.passthrough_args, vec!["-p", "quick verify"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let no_code_mode_codex = parse_strs(&["lg", "cx", "--no-code-mode", "exec", "cargo test"]);
    match no_code_mode_codex.command {
        Some(Commands::Codex(c)) => {
            assert!(c.no_code_mode && !c.code_mode && !c.code_mode_only);
            assert_eq!(c.passthrough_args, vec!["exec", "cargo test"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let code_mode_codex = parse_strs(&["lg", "cx", "--code-mode", "exec", "cargo check"]);
    match code_mode_codex.command {
        Some(Commands::Codex(c)) => {
            assert!(c.code_mode && !c.code_mode_only);
            assert_eq!(c.passthrough_args, vec!["exec", "cargo check"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let code_mode_only_codex = parse_strs(&[
        "lg",
        "cx",
        "--code-mode-only",
        "-m",
        "gemini-4-carbon-high",
        "inspect workspace",
    ]);
    match code_mode_only_codex.command {
        Some(Commands::Codex(c)) => {
            assert!(c.code_mode_only && !c.code_mode);
            assert_eq!(c.model.as_deref(), Some("gemini-4-carbon-high"));
            assert_eq!(c.passthrough_args, vec!["inspect workspace"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn harness_help_stays_with_liftgate_unless_after_double_dash() {
    let norm_own = normalize_harness_args(["liftgate", "claude", "--help"].map(OsString::from));
    assert_eq!(norm_own, vec!["liftgate", "claude", "--help"]);

    let norm_child =
        normalize_harness_args(["liftgate", "claude", "--", "--help"].map(OsString::from));
    assert_eq!(norm_child, vec!["liftgate", "claude", "--", "--help"]);
}

#[test]
fn config_subcommand_defaults_to_all_targets() {
    let cli = parse_strs(&["liftgate", "config", "--global"]);
    match cli.command {
        Some(Commands::Config(cfg)) => {
            assert_eq!(cfg.target, ConfigTarget::All);
            assert!(cfg.global);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn short_subcommand_aliases_and_new_commands_parse_cleanly() {
    let top_cli = parse_strs(&["lg", "tui", "--auto-start"]);
    assert!(matches!(top_cli.command, Some(Commands::Top(t)) if t.auto_start));

    let pick_cli = parse_strs(&["lg", "m", "-i"]);
    assert!(matches!(pick_cli.command, Some(Commands::Models(m)) if m.pick));

    let alias_cli = parse_strs(&["liftgate", "alias", "--name", "lg", "--force"]);
    assert!(matches!(alias_cli.command, Some(Commands::Alias(a)) if a.name == "lg" && a.force));

    let comp_cli = parse_strs(&["lg", "completions", "powershell"]);
    assert!(matches!(
        comp_cli.command,
        Some(Commands::Completions(c)) if c.shell == Shell::PowerShell
    ));

    let upg_cli = parse_strs(&["lg", "upgrade", "--check", "--json"]);
    assert!(matches!(
        upg_cli.command,
        Some(Commands::Upgrade(u)) if u.check && u.json
    ));

    let upd_alias = parse_strs(&["liftgate", "update", "--version", "v0.1.0"]);
    assert!(matches!(
        upd_alias.command,
        Some(Commands::Upgrade(u)) if u.version.as_deref() == Some("v0.1.0")
    ));

    let proxy_set = parse_strs(&["lg", "proxy", "7890"]);
    assert!(matches!(
        proxy_set.command,
        Some(Commands::Proxy(p)) if p.target.as_deref() == Some("7890")
    ));

    let proxy_off = parse_strs(&["lg", "px", "off"]);
    assert!(matches!(
        proxy_off.command,
        Some(Commands::Proxy(p)) if p.target.as_deref() == Some("off")
    ));

    let agy_bare = parse_strs(&["lg", "agy"]);
    assert!(matches!(
        agy_bare.command,
        Some(Commands::Agy(a)) if a.action.is_none()
    ));

    let agy_install = parse_strs(&["lg", "agy", "install", "--force"]);
    assert!(matches!(
        agy_install.command,
        Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Install(i)), .. })) if i.force
    ));

    let agy_login = parse_strs(&["lg", "agy", "login", "--install"]);
    assert!(matches!(
        agy_login.command,
        Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Login(l)), .. })) if l.install
    ));

    let agy_auto_off = parse_strs(&["lg", "agy", "auto-update", "off"]);
    assert!(matches!(
        agy_auto_off.command,
        Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::AutoUpdate(a)), .. })) if a.mode == AutoUpdateToggle::Off
    ));

    let agy_mcp = parse_strs(&["lg", "agy", "mcp", "list"]);
    assert!(matches!(
        agy_mcp.command,
        Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::Mcp(f)), .. })) if f.args == vec![OsString::from("list")]
    ));

    let agy_ext = parse_strs(&["lg", "agy", "agents"]);
    assert!(matches!(
        agy_ext.command,
        Some(Commands::Agy(AgyArgs { action: Some(AgyCommand::External(v)), .. })) if v == vec![OsString::from("agents")]
    ));

    let cxa_cli = parse_strs(&["lg", "cxa", "-m", "gemini-4-carbon-high", "."]);
    match cxa_cli.command {
        Some(Commands::CodexApp(c)) => {
            assert_eq!(c.model.as_deref(), Some("gemini-4-carbon-high"));
            assert_eq!(c.passthrough_args, vec!["."]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cd_cli = parse_strs(&["lg", "cd", "--restart-app", "-w"]);
    assert!(matches!(
        cd_cli.command,
        Some(Commands::ClaudeDesktop(c)) if c.restart_app && c.wait
    ));

    let cx_app = parse_strs(&["lg", "cx", "--app", "--ephemeral"]);
    assert!(matches!(
        cx_app.command,
        Some(Commands::Codex(c)) if c.app && c.ephemeral
    ));

    let cc_desktop = parse_strs(&["lg", "cc", "--desktop", "--restart-app"]);
    assert!(matches!(
        cc_desktop.command,
        Some(Commands::Claude(c)) if c.desktop && c.restart_app
    ));
}

#[test]
fn native_session_commands_mixed_flags_and_conflicting_options_route_cleanly() {
    // Position-independent Liftgate flags after native subcommands
    let cx_resume = parse_strs(&["lg", "cx", "resume", "--last", "-m", "gemini-3.8-pro-high"]);
    match cx_resume.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(c.model.as_deref(), Some("gemini-3.8-pro-high"));
            assert_eq!(c.passthrough_args, vec!["resume", "--last"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // `--ephemeral` without `--app` forwards to native `codex --ephemeral`
    let cx_ephemeral = parse_strs(&["lg", "cx", "--ephemeral", "exec", "cargo check"]);
    match cx_ephemeral.command {
        Some(Commands::Codex(c)) => {
            assert!(!c.app && !c.ephemeral);
            assert_eq!(
                c.passthrough_args,
                vec!["--ephemeral", "exec", "cargo check"]
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // Child `--` inside `codex sandbox` does not confuse top-level clap parsing
    let cx_sandbox = parse_strs(&[
        "lg", "cx", "sandbox", "windows", "--", "cmd", "/c", "echo", "ok",
    ]);
    match cx_sandbox.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(
                c.passthrough_args,
                vec!["sandbox", "windows", "--", "cmd", "/c", "echo", "ok"]
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // Subcommand `--help` forwards to child instead of intercepting as Liftgate help
    let norm_sub_help =
        normalize_harness_args(["lg", "cx", "resume", "--help"].map(OsString::from));
    assert_eq!(norm_sub_help, vec!["lg", "cx", "--", "resume", "--help"]);

    // Codex `-r` / `--resume` / `--continue` ergonomic aliases (preserving `-c` as `--config`)
    let cx_short_r = parse_strs(&["lg", "cx", "-r", "--all"]);
    assert!(matches!(
        cx_short_r.command,
        Some(Commands::Codex(c)) if c.passthrough_args == vec!["resume", "--all"]
    ));
    let cx_cont = parse_strs(&["lg", "cx", "--continue"]);
    assert!(matches!(
        cx_cont.command,
        Some(Commands::Codex(c)) if c.passthrough_args == vec!["resume", "--last"]
    ));
    let cx_cfg = parse_strs(&["lg", "cx", "-c", "model_reasoning_effort='low'", "resume"]);
    assert!(matches!(
        cx_cfg.command,
        Some(Commands::Codex(c)) if c.passthrough_args == vec!["-c", "model_reasoning_effort='low'", "resume"]
    ));

    // Claude `-w` without `--desktop` forwards to native `claude --worktree`, while `--desktop -w` sets `wait`
    let cc_worktree = parse_strs(&["lg", "cc", "-w", "feat-branch"]);
    match cc_worktree.command {
        Some(Commands::Claude(c)) => {
            assert!(!c.desktop && !c.wait);
            assert_eq!(c.passthrough_args, vec!["-w", "feat-branch"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let cc_desktop_wait = parse_strs(&["lg", "cc", "--desktop", "-w"]);
    assert!(matches!(
        cc_desktop_wait.command,
        Some(Commands::Claude(c)) if c.desktop && c.wait
    ));

    // Claude `resume` / `continue` / `fork` aliases + trailing `-m` hoisting
    let cc_resume = parse_strs(&[
        "lg",
        "cc",
        "resume",
        "abc-123",
        "-m",
        "gemini-3.8-flash-low",
    ]);
    match cc_resume.command {
        Some(Commands::Claude(c)) => {
            assert_eq!(c.model.as_deref(), Some("gemini-3.8-flash-low"));
            assert_eq!(c.passthrough_args, vec!["--resume", "abc-123"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let cc_resume_last = parse_strs(&["lg", "cc", "resume", "--last"]);
    assert!(matches!(
        cc_resume_last.command,
        Some(Commands::Claude(c)) if c.passthrough_args == vec!["--continue"]
    ));
    let cc_fork = parse_strs(&["lg", "cc", "fork", "sess-42"]);
    assert!(matches!(
        cc_fork.command,
        Some(Commands::Claude(c)) if c.passthrough_args == vec!["--resume", "sess-42", "--fork-session"]
    ));

    // Native `claude --continue --desktop` session handoff passes through instead of launching raw 3P desktop
    let cc_handoff = parse_strs(&["lg", "cc", "-c", "--desktop"]);
    match cc_handoff.command {
        Some(Commands::Claude(c)) => {
            assert!(!c.desktop);
            assert_eq!(c.passthrough_args, vec!["-c", "--desktop"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // OpenCode (`lg oc` / `lg opencode`) argument hoisting & session aliases
    let oc_run = parse_strs(&[
        "lg",
        "oc",
        "run",
        "fix failing tests",
        "-m",
        "gemini-4-carbon-high",
        "--full-code-mode",
    ]);
    match oc_run.command {
        Some(Commands::Opencode(oc)) => {
            assert_eq!(oc.model.as_deref(), Some("gemini-4-carbon-high"));
            assert!(oc.full_code_mode);
            assert!(!oc.no_code_mode);
            assert_eq!(oc.passthrough_args, vec!["run", "fix failing tests"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let oc_mcp_cm = parse_strs(&["lg", "oc", "run", "inspect mcp", "--mcp-code-mode"]);
    match oc_mcp_cm.command {
        Some(Commands::Opencode(oc)) => {
            assert!(oc.mcp_code_mode);
            assert!(!oc.full_code_mode);
            assert!(!oc.no_code_mode);
            assert_eq!(oc.passthrough_args, vec!["run", "inspect mcp"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let oc_resume_id = parse_strs(&["lg", "oc", "resume", "ses_42"]);
    assert!(matches!(
        oc_resume_id.command,
        Some(Commands::Opencode(oc)) if oc.passthrough_args == vec!["--session", "ses_42"]
    ));
    let oc_continue = parse_strs(&["lg", "opencode", "continue"]);
    assert!(matches!(
        oc_continue.command,
        Some(Commands::Opencode(oc)) if oc.passthrough_args == vec!["--continue"]
    ));
    let cfg_oc = parse_strs(&["lg", "config", "opencode", "--global"]);
    assert!(matches!(
        cfg_oc.command,
        Some(Commands::Config(c)) if c.target == ConfigTarget::Opencode && c.global
    ));
}

#[test]
fn omp_subcommand_normalizes_flags_and_session_aliases() {
    let omp_launch = parse_strs(&[
        "lg",
        "omp",
        "-r",
        "sess-99",
        "-m",
        "gemini-4-carbon-high",
        "--full-code-mode",
    ]);
    match omp_launch.command {
        Some(Commands::Omp(omp)) => {
            assert_eq!(omp.model.as_deref(), Some("gemini-4-carbon-high"));
            assert!(omp.full_code_mode);
            assert!(!omp.no_code_mode);
            assert_eq!(omp.passthrough_args, vec!["-r", "sess-99"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let omp_resume_id = parse_strs(&["lg", "omp", "resume", "sess-42"]);
    assert!(matches!(
        omp_resume_id.command,
        Some(Commands::Omp(omp)) if omp.passthrough_args == vec!["--resume", "sess-42"]
    ));
    let omp_resume_last = parse_strs(&["lg", "omp", "resume", "--last"]);
    assert!(matches!(
        omp_resume_last.command,
        Some(Commands::Omp(omp)) if omp.passthrough_args == vec!["--continue"]
    ));
    let omp_continue = parse_strs(&["lg", "oh-my-pi", "continue"]);
    assert!(matches!(
        omp_continue.command,
        Some(Commands::Omp(omp)) if omp.passthrough_args == vec!["--continue"]
    ));
    let cfg_omp = parse_strs(&["lg", "config", "omp", "--global"]);
    assert!(matches!(
        cfg_omp.command,
        Some(Commands::Config(c)) if c.target == ConfigTarget::Omp && c.global
    ));
    let cfg_alias = parse_strs(&["lg", "config", "oh-my-pi"]);
    assert!(matches!(
        cfg_alias.command,
        Some(Commands::Config(c)) if c.target == ConfigTarget::Omp && !c.global
    ));
}

#[test]
fn remote_subcommand_and_harness_remote_flag_normalize_without_codex_collision() {
    let rem_set = parse_strs(&[
        "lg",
        "remote",
        "192.168.1.50:8787",
        "--api-key",
        "tok-123",
        "--no-verify",
    ]);
    match rem_set.command {
        Some(Commands::Remote(r)) => {
            assert_eq!(r.target.as_deref(), Some("192.168.1.50:8787"));
            assert_eq!(r.api_key.as_deref(), Some("tok-123"));
            assert!(r.no_verify);
            assert!(!r.off);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let rem_off = parse_strs(&["lg", "endpoint", "--local"]);
    assert!(matches!(
        rem_off.command,
        Some(Commands::Remote(r)) if r.off
    ));

    // `lg cx --remote <host:port> resume --last` routes `--remote` to Liftgate `CodexArgs`,
    // whereas `lg cx -- --remote ws://remote-tui` forwards `--remote` to native `codex`!
    let cx_remote = parse_strs(&[
        "lg",
        "cx",
        "--remote",
        "192.168.1.50:8787",
        "resume",
        "--last",
    ]);
    match cx_remote.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(c.remote.as_deref(), Some("192.168.1.50:8787"));
            assert_eq!(c.passthrough_args, vec!["resume", "--last"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cx_native_remote = parse_strs(&["lg", "cx", "--", "--remote", "ws://127.0.0.1:9999"]);
    match cx_native_remote.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(c.remote, None);
            assert_eq!(c.passthrough_args, vec!["--remote", "ws://127.0.0.1:9999"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let omp_remote = parse_strs(&["lg", "omp", "--remote=devbox.lan:8787", "-c"]);
    match omp_remote.command {
        Some(Commands::Omp(o)) => {
            assert_eq!(o.remote.as_deref(), Some("devbox.lan:8787"));
            assert_eq!(o.passthrough_args, vec!["-c"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn auto_update_cli_commands_and_lg_auto_update_flags_parse_cleanly() {
    use super::{AutoUpdateCommand, Commands};

    let bare = parse_strs(&["lg", "auto-update"]);
    assert!(matches!(bare.command, Some(Commands::AutoUpdate(a)) if a.action.is_none() && !a.json));

    let alias_tui = parse_strs(&["lg", "au", "tui"]);
    assert!(matches!(
        alias_tui.command,
        Some(Commands::AutoUpdate(a)) if matches!(a.action, Some(AutoUpdateCommand::Tui))
    ));

    let enable = parse_strs(&["lg", "au", "on", "codex", "omp"]);
    match enable.command {
        Some(Commands::AutoUpdate(a)) => match a.action {
            Some(AutoUpdateCommand::Enable(t)) => {
                assert_eq!(t.targets, vec!["codex", "omp"]);
                assert!(!t.all);
            }
            other => panic!("unexpected action: {other:?}"),
        },
        other => panic!("unexpected command: {other:?}"),
    }

    let interval = parse_strs(&["lg", "auto-update", "interval", "60"]);
    assert!(matches!(
        interval.command,
        Some(Commands::AutoUpdate(a))
            if matches!(a.action, Some(AutoUpdateCommand::Interval(iv)) if iv.minutes == 60)
    ));

    let restart = parse_strs(&["lg", "restart", "--no-lg-auto-update"]);
    assert!(matches!(
        restart.command,
        Some(Commands::Restart(r)) if r.no_lg_auto_update && r.lg_auto_update.is_none()
    ));

    let cx_after_flag = parse_strs(&["lg", "--lg-auto-update", "false", "cx", "resume"]);
    match cx_after_flag.command {
        Some(Commands::Codex(c)) => {
            assert_eq!(cx_after_flag.serve.lg_auto_update, Some(false));
            assert_eq!(c.passthrough_args, vec!["resume"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}
