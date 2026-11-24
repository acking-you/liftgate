//! Shared cross-platform executable and Windows shim resolver for all supported coding harnesses.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::HarnessError;
#[cfg(not(windows))]
use crate::fs_util::user_home_dir;
use crate::fs_util::{canonicalize_clean, is_executable_file, split_expanded_path_var};

/// Resolved child CLI program and any shim prefix arguments (such as `cmd.exe /d /c <shim.cmd>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub program: PathBuf,
    pub prefix_args: Vec<PathBuf>,
}

/// Declarative per-harness executable lookup specification consumed by [`resolve_harness_command`].
pub(crate) struct HarnessLookupSpec<'a> {
    pub tool: &'static str,
    pub explicit_invalid_hint: &'static str,
    pub missing_install_hint: &'static str,
    pub unix_bin_name: &'static str,
    pub unix_home_rel_paths: &'a [&'a [&'static str]],
    pub windows_exe_name: &'static str,
    pub windows_extra_exe_candidates: Vec<PathBuf>,
    pub windows_npm_script_rel: Option<&'a [&'static str]>,
    pub windows_cmd_shims: &'a [&'static str],
    pub windows_ps1_shims: &'a [&'static str],
}

/// Resolves a harness CLI command using one unified precedence order across all harnesses:
/// 1. Explicit `--<harness>-bin` override (fails immediately if non-executable; never falls back).
/// 2. Native executable in `PATH` (`<bin>.exe` on Windows, `<bin>` on Unix).
/// 3. Well-known user/OS installation directories.
/// 4. Optional Windows `node.exe <npm_package_entry.js>` direct launch (bypassing `cmd.exe` quoting limits).
/// 5. Windows `.cmd` / `.bat` shim via `%ComSpec% /d /c`.
/// 6. Windows `.ps1` shim via `powershell.exe -NoProfile -ExecutionPolicy Bypass -File`.
pub(crate) fn resolve_harness_command(
    explicit: Option<&Path>,
    spec: &HarnessLookupSpec<'_>,
) -> Result<ResolvedCommand, HarnessError> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Ok(ResolvedCommand {
                program: canonicalize_clean(p),
                prefix_args: Vec::new(),
            });
        }
        return Err(HarnessError::HarnessNotFound {
            tool: spec.tool,
            hint: spec.explicit_invalid_hint,
        });
    }

    let path_dirs = split_expanded_path_var();

    #[cfg(windows)]
    {
        let _ = (spec.unix_bin_name, spec.unix_home_rel_paths);
        let comspec =
            PathBuf::from(std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe")));
        if let Some(cmd) = resolve_windows_in_env(&path_dirs, spec, &comspec) {
            return Ok(cmd);
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (
            spec.windows_exe_name,
            &spec.windows_extra_exe_candidates,
            spec.windows_npm_script_rel,
            spec.windows_cmd_shims,
            spec.windows_ps1_shims,
            OsString::new(),
        );
        if let Some(bin) = find_in_dirs(&path_dirs, &[spec.unix_bin_name]) {
            return Ok(ResolvedCommand {
                program: bin,
                prefix_args: Vec::new(),
            });
        }
        if let Ok(home) = user_home_dir() {
            for rel in spec.unix_home_rel_paths {
                let mut cand = home.clone();
                for seg in *rel {
                    cand.push(seg);
                }
                if is_executable_file(&cand) {
                    return Ok(ResolvedCommand {
                        program: canonicalize_clean(&cand),
                        prefix_args: Vec::new(),
                    });
                }
            }
        }
        for cand in unix_extra_bin_candidates(spec.unix_bin_name) {
            if is_executable_file(&cand) {
                return Ok(ResolvedCommand {
                    program: canonicalize_clean(&cand),
                    prefix_args: Vec::new(),
                });
            }
        }
    }

    Err(HarnessError::HarnessNotFound {
        tool: spec.tool,
        hint: spec.missing_install_hint,
    })
}

#[cfg(any(windows, test))]
fn resolve_windows_in_env(
    path_dirs: &[PathBuf],
    spec: &HarnessLookupSpec<'_>,
    comspec: &Path,
) -> Option<ResolvedCommand> {
    if let Some(exe) = find_in_dirs(path_dirs, &[spec.windows_exe_name]) {
        return Some(ResolvedCommand {
            program: exe,
            prefix_args: Vec::new(),
        });
    }
    for cand in &spec.windows_extra_exe_candidates {
        if is_executable_file(cand) {
            return Some(ResolvedCommand {
                program: canonicalize_clean(cand),
                prefix_args: Vec::new(),
            });
        }
    }
    if let Some(rel_segments) = spec.windows_npm_script_rel
        && let Some(node_cmd) = resolve_windows_npm_script(path_dirs, rel_segments)
    {
        return Some(node_cmd);
    }
    if !spec.windows_cmd_shims.is_empty()
        && let Some(shim) = find_in_dirs(path_dirs, spec.windows_cmd_shims)
    {
        return Some(ResolvedCommand {
            program: comspec.to_path_buf(),
            prefix_args: vec![PathBuf::from("/d"), PathBuf::from("/c"), shim],
        });
    }
    if !spec.windows_ps1_shims.is_empty()
        && let Some(ps1) = find_in_dirs(path_dirs, spec.windows_ps1_shims)
    {
        return Some(ResolvedCommand {
            program: PathBuf::from("powershell.exe"),
            prefix_args: vec![
                PathBuf::from("-NoProfile"),
                PathBuf::from("-ExecutionPolicy"),
                PathBuf::from("Bypass"),
                PathBuf::from("-File"),
                ps1,
            ],
        });
    }
    None
}

fn find_in_dirs(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let cand = dir.join(name);
            if is_executable_file(&cand) {
                return Some(canonicalize_clean(&cand));
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn unix_extra_bin_candidates(bin_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = user_home_dir() {
        out.push(home.join(".bun").join("bin").join(bin_name));
        out.push(home.join(".local").join("bin").join(bin_name));
        out.push(home.join(".npm-global").join("bin").join(bin_name));
        out.push(home.join(".cargo").join("bin").join(bin_name));
    }
    out.push(PathBuf::from("/opt/homebrew/bin").join(bin_name));
    out.push(PathBuf::from("/usr/local/bin").join(bin_name));
    out
}

#[cfg(any(windows, test))]
fn resolve_windows_npm_script(
    path_dirs: &[PathBuf],
    rel_segments: &[&str],
) -> Option<ResolvedCommand> {
    let mut candidate_roots: Vec<PathBuf> = path_dirs.to_vec();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidate_roots.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(nvm) = std::env::var_os("NVM_SYMLINK") {
        candidate_roots.push(PathBuf::from(nvm));
    }
    candidate_roots.push(PathBuf::from(r"C:\nvm4w\nodejs"));

    for root in &candidate_roots {
        let mut script_path = root.clone();
        for seg in rel_segments {
            script_path.push(seg);
        }
        if script_path.is_file()
            && let Some(node_exe) = resolve_windows_node_exe(Some(root), path_dirs)
        {
            return Some(ResolvedCommand {
                program: node_exe,
                prefix_args: vec![canonicalize_clean(&script_path)],
            });
        }
    }
    None
}

#[cfg(any(windows, test))]
fn resolve_windows_node_exe(
    preferred_dir: Option<&Path>,
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(dir) = preferred_dir {
        let local = dir.join("node.exe");
        if is_executable_file(&local) {
            return Some(canonicalize_clean(&local));
        }
    }
    if let Some(found) = find_in_dirs(path_dirs, &["node.exe"]) {
        return Some(found);
    }
    let mut fallbacks = Vec::new();
    if let Some(nvm) = std::env::var_os("NVM_SYMLINK") {
        fallbacks.push(PathBuf::from(nvm).join("node.exe"));
    }
    fallbacks.push(PathBuf::from(r"C:\nvm4w\nodejs\node.exe"));
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        fallbacks.push(PathBuf::from(pf).join("nodejs").join("node.exe"));
    }
    if let Some(volta) = std::env::var_os("VOLTA_HOME") {
        fallbacks.push(PathBuf::from(volta).join("bin").join("node.exe"));
    }
    for cand in fallbacks {
        if is_executable_file(&cand) {
            return Some(canonicalize_clean(&cand));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use uuid::Uuid;

    use super::*;

    fn touch_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn shared_resolver_enforces_explicit_path_and_windows_shim_precedence() {
        let tmp = std::env::temp_dir().join(format!("lg-resolve-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();

        let spec = HarnessLookupSpec {
            tool: "omp",
            explicit_invalid_hint: "explicit `--omp-bin` path does not exist or is not executable",
            missing_install_hint: "install Oh My Pi",
            unix_bin_name: "omp",
            unix_home_rel_paths: &[&[".omp", "bin", "omp"]],
            windows_exe_name: "omp.exe",
            windows_extra_exe_candidates: Vec::new(),
            windows_npm_script_rel: None,
            windows_cmd_shims: &["omp.cmd", "omp.bat"],
            windows_ps1_shims: &["omp.ps1"],
        };

        // 1. Missing explicit path fails immediately without scanning PATH.
        let missing = tmp.join("missing-omp");
        assert!(matches!(
            resolve_harness_command(Some(&missing), &spec),
            Err(HarnessError::HarnessNotFound { tool: "omp", .. })
        ));

        // 2. `.ps1` fallback builds PowerShell invocation.
        let ps1 = tmp.join("omp.ps1");
        touch_executable(&ps1);
        let comspec = PathBuf::from("C:\\Windows\\System32\\cmd.exe");
        let resolved_ps1 =
            resolve_windows_in_env(std::slice::from_ref(&tmp), &spec, &comspec).unwrap();
        assert_eq!(resolved_ps1.program, PathBuf::from("powershell.exe"));
        assert_eq!(
            resolved_ps1.prefix_args.last(),
            Some(&canonicalize_clean(&ps1))
        );

        // 3. `.cmd` takes priority over `.ps1`.
        let cmd_shim = tmp.join("omp.cmd");
        touch_executable(&cmd_shim);
        let resolved_cmd =
            resolve_windows_in_env(std::slice::from_ref(&tmp), &spec, &comspec).unwrap();
        assert_eq!(resolved_cmd.program, comspec);
        assert_eq!(
            resolved_cmd.prefix_args,
            vec![
                PathBuf::from("/d"),
                PathBuf::from("/c"),
                canonicalize_clean(&cmd_shim)
            ]
        );

        // 4. Native `.exe` takes priority over `.cmd` and `.ps1`.
        let exe = tmp.join("omp.exe");
        touch_executable(&exe);
        let resolved_exe =
            resolve_windows_in_env(std::slice::from_ref(&tmp), &spec, &comspec).unwrap();
        assert_eq!(resolved_exe.program, canonicalize_clean(&exe));
        assert!(resolved_exe.prefix_args.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }
}
