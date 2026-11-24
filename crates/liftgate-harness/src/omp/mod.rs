//! Oh My Pi (`omp` / `@oh-my-pi/pi-coding-agent`) configuration, provider schema, and binary adapter.

pub mod config;
pub mod invocation;
pub mod provider;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::HarnessError;
use crate::fs_util::user_home_dir;
use crate::resolve::{HarnessLookupSpec, ResolvedCommand, resolve_harness_command};

pub use config::{
    OmpResolvedPaths, build_omp_ephemeral_overlay_yaml, extract_omp_yaml_extensions,
    has_omp_agy_provider_yaml, inspect_omp_default_model, merge_omp_models_yaml, resolve_omp_paths,
    revert_omp_models_yaml, uninstall_omp_config, uninstall_omp_config_with_ledger,
    write_or_update_omp_config, write_or_update_omp_config_with_ledger,
};
pub use invocation::*;
pub use provider::{
    build_omp_provider_extension_ts, build_omp_provider_json, render_omp_provider_agy_yaml_block,
    render_omp_provider_agy_yaml_block_indented,
};

/// Provider identifier registered in `models.yml` and `pi.registerProvider("agy", ...)`.
pub const OMP_PROVIDER_ID: &str = "agy";

/// Ephemeral environment variable carrying the gateway API key for `lg omp`.
pub const OMP_EPHEMERAL_API_KEY_ENV: &str = "AGY_OMP_API_KEY";

/// Project-local TypeScript extension path written into `.omp/config.yml`.
pub const OMP_LOCAL_EXTENSION_REL_PATH: &str = "./.omp/liftgate-agy-provider.ts";

/// Header sentinel identifying Liftgate-managed `.omp/liftgate-agy-provider.ts` files.
pub const OMP_MANAGED_EXTENSION_SENTINEL: &str = "// Managed by Liftgate (`lg config omp`)";

pub(crate) const OMP_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;
pub(crate) const OMP_LOCAL_EXTENSION_FILENAME: &str = "liftgate-agy-provider.ts";
pub(crate) const OMP_PRIMARY_ROLE_KEYS: &[&str] = &["default", "slow", "plan", "task"];
pub(crate) const OMP_FAST_ROLE_KEYS: &[&str] = &["smol", "commit", "tiny", "memory", "widget"];

/// Resolves the `omp` CLI executable using the shared cross-platform harness resolver.
pub fn resolve_omp_command(explicit: Option<&Path>) -> Result<ResolvedCommand, HarnessError> {
    let spec = HarnessLookupSpec {
        tool: "omp",
        explicit_invalid_hint: "explicit `--omp-bin` path does not exist or is not executable",
        missing_install_hint: "install Oh My Pi (`bun install -g @oh-my-pi/pi-coding-agent` or `curl -fsSL https://omp.sh/install | sh`) or pass `--omp-bin <path>`",
        unix_bin_name: "omp",
        unix_home_rel_paths: &[&[".omp", "bin", "omp"]],
        windows_exe_name: "omp.exe",
        windows_extra_exe_candidates: windows_omp_exe_candidates(),
        windows_npm_script_rel: None,
        windows_cmd_shims: &["omp.cmd", "omp.bat"],
        windows_ps1_shims: &["omp.ps1"],
    };
    resolve_harness_command(explicit, &spec)
}

fn windows_omp_exe_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".omp").join("bin").join("omp.exe"));
        out.push(home.join(".bun").join("bin").join("omp.exe"));
        out.push(home.join(".local").join("bin").join("omp.exe"));
        out.push(home.join("scoop").join("shims").join("omp.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local_path = PathBuf::from(local);
        out.push(
            local_path
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("omp.exe"),
        );
        out.push(local_path.join("Programs").join("omp").join("omp.exe"));
    }
    out
}

/// Collects existing `extensions` entries across global and project `omp` YAML configs so an
/// ephemeral `PI_CONFIG_FILES` overlay inherits user extensions without shadowing them.
#[must_use]
pub fn collect_existing_omp_extensions(cwd: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(global_paths) = resolve_omp_paths(true, None)
        && let Ok(text) = fs::read_to_string(&global_paths.config_path)
    {
        out.extend(extract_omp_yaml_extensions(&text));
    }
    if let Ok(local_paths) = resolve_omp_paths(false, cwd)
        && let Ok(text) = fs::read_to_string(&local_paths.config_path)
    {
        for item in extract_omp_yaml_extensions(&text) {
            if !out.iter().any(|existing| existing == &item) {
                out.push(item);
            }
        }
    }
    out
}

/// Appends `overlay_path` to an optional existing `PI_CONFIG_FILES` path list (`:` on Unix, `;` on Windows).
#[must_use]
pub fn merge_omp_config_files_env(existing: Option<&OsStr>, overlay_path: &Path) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let overlay_str = overlay_path.to_string_lossy();
    let Some(raw) = existing.and_then(OsStr::to_str) else {
        return overlay_str.into_owned();
    };
    let mut entries: Vec<&str> = raw
        .split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !entries.contains(&overlay_str.as_ref()) {
        entries.push(overlay_str.as_ref());
    }
    entries.join(sep)
}

#[cfg(test)]
mod tests;
