use std::ffi::OsString;
use std::path::PathBuf;

use liftgate_harness::args_util::extract_explicit_model_arg;
use liftgate_harness::claude::{
    ClaudeInvocationKind, classify_claude_invocation, default_claude_effort_level,
};
use liftgate_harness::codex::{
    CodexInvocationKind, classify_codex_invocation, has_explicit_codex_config_key,
    toml_inline_string,
};
use liftgate_harness::omp::merge_omp_config_files_env;
use liftgate_harness::opencode::{
    has_negated_opencode_flag, merge_opencode_flag, parse_opencode_major_version,
};

use super::*;
use super::{
    codex::build_codex_invocation,
    omp::build_omp_invocation,
    opencode::{apply_opencode_runtime_env_flags, build_opencode_invocation},
};
use crate::cli::{CodexArgs, OmpArgs, OpencodeArgs};
use crate::state::StatePaths;

#[test]
fn toml_inline_string_uses_single_quotes_for_plain_values() {
    assert_eq!(
        toml_inline_string("gemini-4-carbon-high"),
        "'gemini-4-carbon-high'"
    );
    assert_eq!(
        toml_inline_string("C:/Users/alice/.liftgate/ephemeral/cat.json"),
        "'C:/Users/alice/.liftgate/ephemeral/cat.json'"
    );
}

#[test]
fn toml_inline_string_escapes_embedded_single_quotes_as_json_basic_string() {
    assert_eq!(
        toml_inline_string("C:/Users/O'Connor/cat.json"),
        "\"C:/Users/O'Connor/cat.json\""
    );
}

#[test]
fn resolve_explicit_nonexistent_binary_returns_not_found() {
    let missing = PathBuf::from("__nonexistent_liftgate_test_binary__.exe");
    assert!(matches!(
        resolve_claude_command(Some(&missing)),
        Err(CliError::HarnessNotFound { tool: "claude", .. })
    ));
    assert!(matches!(
        resolve_codex_command(Some(&missing)),
        Err(CliError::HarnessNotFound { tool: "codex", .. })
    ));
}

#[test]
fn infers_default_claude_effort_level_from_model_suffix() {
    assert_eq!(
        default_claude_effort_level("gemini-4-neon-xhigh"),
        Some("xhigh")
    );
    assert_eq!(
        default_claude_effort_level("gemini-4-carbon-high"),
        Some("high")
    );
    assert_eq!(
        default_claude_effort_level("claude-opus-4-6-thinking"),
        Some("high")
    );
    assert_eq!(
        default_claude_effort_level("gemini-4-argon-medium"),
        Some("medium")
    );
    assert_eq!(
        default_claude_effort_level("gemini-3.8-flash-low"),
        Some("low")
    );
    assert_eq!(default_claude_effort_level("claude-sonnet-4-6"), None);
}

#[test]
fn classifies_claude_invocations_across_sessions_jobs_and_utilities() {
    let strs = |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).to_owned()).collect() };

    assert_eq!(
        classify_claude_invocation(&strs(&[])),
        ClaudeInvocationKind::AgentSession
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["--resume", "abc"])),
        ClaudeInvocationKind::AgentSession
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["-c", "-p", "continue work"])),
        ClaudeInvocationKind::AgentSession
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["fix failing tests"])),
        ClaudeInvocationKind::AgentSession
    );

    for sub in [
        "logs", "attach", "stop", "kill", "rm", "mcp", "plugin", "purge",
    ] {
        assert_eq!(
            classify_claude_invocation(&strs(&[sub, "target"])),
            ClaudeInvocationKind::DirectPassthrough,
            "expected DirectPassthrough for claude {sub}"
        );
    }
    assert_eq!(
        classify_claude_invocation(&strs(&["agents", "--json"])),
        ClaudeInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["agents"])),
        ClaudeInvocationKind::AgentsView
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["--version"])),
        ClaudeInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_claude_invocation(&strs(&["--resume", "--help"])),
        ClaudeInvocationKind::DirectPassthrough
    );
    for sub in ["respawn", "doctor", "ultrareview"] {
        assert_eq!(
            classify_claude_invocation(&strs(&[sub])),
            ClaudeInvocationKind::GatewayEnvSubcommand,
            "expected GatewayEnvSubcommand for claude {sub}"
        );
    }
}

#[test]
fn classifies_codex_invocations_and_root_options_accurately() {
    let strs = |items: &[&str]| -> Vec<String> { items.iter().map(|s| (*s).to_owned()).collect() };

    assert_eq!(
        classify_codex_invocation(&strs(&[])),
        CodexInvocationKind::InteractiveSession
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["explain architecture"])),
        CodexInvocationKind::InteractiveSession
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["resume", "--last"])),
        CodexInvocationKind::InteractiveSession
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["-C", "../other", "resume", "--all"])),
        CodexInvocationKind::InteractiveSession
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["fork", "--last"])),
        CodexInvocationKind::InteractiveSession
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["exec", "resume", "--last", "test"])),
        CodexInvocationKind::ExecSession
    );

    assert_eq!(
        classify_codex_invocation(&strs(&["agents"])),
        CodexInvocationKind::AgentsView
    );
    for sub in ["review", "doctor", "debug"] {
        assert_eq!(
            classify_codex_invocation(&strs(&[sub])),
            CodexInvocationKind::GatewayConfigOnly,
            "expected GatewayConfigOnly for codex {sub}"
        );
    }

    for sub in [
        "queue",
        "archive",
        "unarchive",
        "delete",
        "mcp",
        "plugin",
        "features",
        "completion",
        "sandbox",
        "login",
        "logout",
    ] {
        assert_eq!(
            classify_codex_invocation(&strs(&[sub])),
            CodexInvocationKind::DirectPassthrough,
            "expected DirectPassthrough for codex {sub}"
        );
    }
    assert_eq!(
        classify_codex_invocation(&strs(&["app-server", "daemon", "version"])),
        CodexInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["app-server", "generate-ts"])),
        CodexInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["app-server"])),
        CodexInvocationKind::GatewayConfigOnly
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["exec", "help"])),
        CodexInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["resume", "--help"])),
        CodexInvocationKind::DirectPassthrough
    );
    assert_eq!(
        classify_codex_invocation(&strs(&["-V"])),
        CodexInvocationKind::DirectPassthrough
    );

    assert_eq!(
        extract_explicit_model_arg(&strs(&["resume", "-m", "gemini-3.8-pro-high"])),
        Some("gemini-3.8-pro-high")
    );
    assert!(has_explicit_codex_config_key(
        &strs(&["-c", "model = 'gemini-3.8-flash-low'", "resume"]),
        "model"
    ));
}

#[tokio::test]
async fn codex_terminal_session_disables_updates_after_passthrough_overrides()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    let state_paths = StatePaths::resolve(Some(
        std::env::temp_dir().join(format!("liftgate-codex-{}", uuid::Uuid::new_v4())),
    ))?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let args = CodexArgs {
        listen: Some(listener.local_addr()?),
        api_key: Some("test-gateway-key".into()),
        codex_bin: Some(std::env::current_exe()?),
        passthrough_args: [
            "resume",
            "-c",
            "check_for_update_on_startup=true",
            "--",
            "-prompt",
        ]
        .map(str::to_owned)
        .to_vec(),
        ..CodexArgs::default_interactive()
    };
    let serve_gateway = async {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let (spec, served) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            prepare_codex_terminal_spec(&state_paths, &args, None, false),
            serve_gateway,
        )
    })
    .await?;
    let spec = spec?;
    served?;
    std::fs::remove_dir_all(state_paths.root())?;

    let command = command_from_terminal_spec(&spec);
    let command = command.as_std();
    let argv = command.get_args().collect::<Vec<_>>();
    assert!(
        argv.ends_with(
            &[
                "resume",
                "-c",
                "check_for_update_on_startup=true",
                "-c",
                "check_for_update_on_startup=false",
                "--",
                "-prompt"
            ]
            .map(std::ffi::OsStr::new)
        )
    );
    assert!(command.get_envs().any(|(key, value)| {
        key == "AGY_GATEWAY_API_KEY" && value == Some(std::ffi::OsStr::new("test-gateway-key"))
    }));
    Ok(())
}

#[tokio::test]
async fn explicit_codex_update_uses_the_native_environment()
-> Result<(), Box<dyn std::error::Error>> {
    let state_paths = StatePaths::resolve(Some(PathBuf::from("unused-codex-update-test-state")))?;
    let args = CodexArgs {
        codex_bin: Some(std::env::current_exe()?),
        passthrough_args: vec!["update".to_owned()],
        ..CodexArgs::default_interactive()
    };
    let (spec, catalog) = build_codex_invocation(&state_paths, &args, None, false).await?;
    let command = command_from_terminal_spec(&spec);
    assert_eq!(
        command.as_std().get_args().collect::<Vec<_>>(),
        ["update"].map(std::ffi::OsStr::new)
    );
    assert_eq!(command.as_std().get_envs().count(), 0);
    assert!(catalog.is_none());
    Ok(())
}

#[tokio::test]
async fn opencode_direct_passthrough_subcommands_do_not_start_gateway()
-> Result<(), Box<dyn std::error::Error>> {
    let state_paths =
        StatePaths::resolve(Some(PathBuf::from("unused-opencode-passthrough-state")))?;
    let args = OpencodeArgs {
        opencode_bin: Some(std::env::current_exe()?),
        passthrough_args: vec!["mcp".to_owned(), "list".to_owned()],
        ..OpencodeArgs::default_interactive()
    };
    let spec = build_opencode_invocation(&state_paths, &args, None, false).await?;
    assert_eq!(
        spec.args,
        vec![OsString::from("mcp"), OsString::from("list")]
    );
    assert!(spec.env_set.is_empty());
    Ok(())
}

#[test]
fn parses_opencode_major_versions_and_merges_flags() {
    assert_eq!(parse_opencode_major_version("2.3.2\n"), Some(2));
    assert_eq!(parse_opencode_major_version("opencode v1.4.12"), Some(1));
    assert_eq!(parse_opencode_major_version("dev-snapshot"), None);
    assert_eq!(merge_opencode_flag(None, "full_codemode"), "full_codemode");
    assert_eq!(
        merge_opencode_flag(Some("sqlite_fts,-full_codemode"), "full_codemode"),
        "sqlite_fts,full_codemode"
    );
    assert!(has_negated_opencode_flag(
        Some("sqlite_fts, -full_codemode"),
        "full_codemode"
    ));
    assert!(!has_negated_opencode_flag(
        Some("sqlite_fts,full_codemode"),
        "full_codemode"
    ));

    // OpenCode v2 default enables `OPENCODE_FLAGS=full_codemode` automatically
    let mut v2_default_env = Vec::new();
    apply_opencode_runtime_env_flags(
        &mut v2_default_env,
        &OpencodeArgs::default_interactive(),
        true,
        Some("sqlite_fts"),
        false,
        false,
    );
    assert_eq!(
        v2_default_env,
        vec![(
            "OPENCODE_FLAGS".to_owned(),
            "sqlite_fts,full_codemode".to_owned()
        )]
    );

    // `--mcp-code-mode` strips `full_codemode` (`-full_codemode`) while keeping `execute` enabled
    let mut v2_mcp_env = Vec::new();
    let mcp_args = OpencodeArgs {
        mcp_code_mode: true,
        ..OpencodeArgs::default_interactive()
    };
    apply_opencode_runtime_env_flags(
        &mut v2_mcp_env,
        &mcp_args,
        true,
        Some("full_codemode"),
        false,
        false,
    );
    assert_eq!(
        v2_mcp_env,
        vec![("OPENCODE_FLAGS".to_owned(), "-full_codemode".to_owned())]
    );

    // OpenCode v1 default enables `OPENCODE_EXPERIMENTAL_CODE_MODE=true` unless already set
    let mut v1_default_env = Vec::new();
    apply_opencode_runtime_env_flags(
        &mut v1_default_env,
        &OpencodeArgs::default_interactive(),
        false,
        None,
        false,
        false,
    );
    assert_eq!(
        v1_default_env,
        vec![(
            "OPENCODE_EXPERIMENTAL_CODE_MODE".to_owned(),
            "true".to_owned()
        )]
    );
}

#[tokio::test]
async fn omp_direct_passthrough_subcommands_do_not_start_gateway()
-> Result<(), Box<dyn std::error::Error>> {
    let state_paths = StatePaths::resolve(Some(PathBuf::from("unused-omp-passthrough-state")))?;
    for sub_args in [
        vec!["stats".to_owned()],
        vec!["plugin".to_owned(), "list".to_owned()],
        vec!["--version".to_owned()],
    ] {
        let args = OmpArgs {
            omp_bin: Some(std::env::current_exe()?),
            passthrough_args: sub_args.clone(),
            ..OmpArgs::default_interactive()
        };
        let (spec, guards) = build_omp_invocation(&state_paths, &args, None, false).await?;
        assert!(guards.is_empty());
        assert_eq!(
            spec.args,
            sub_args.into_iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert!(spec.env_set.is_empty());
    }
    Ok(())
}

#[test]
fn merges_pi_config_files_preserving_existing_entries() {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let overlay = Path::new("omp-overlay-test.yml");
    assert_eq!(
        merge_omp_config_files_env(None, overlay),
        "omp-overlay-test.yml"
    );
    let existing = OsString::from(format!("first.yml{sep}second.yml"));
    assert_eq!(
        merge_omp_config_files_env(Some(existing.as_os_str()), overlay),
        format!("first.yml{sep}second.yml{sep}omp-overlay-test.yml")
    );
}
