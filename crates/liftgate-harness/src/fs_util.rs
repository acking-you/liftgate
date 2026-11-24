//! Cross-platform path canonicalization, atomic file replacement, and JSON/JSONC helpers.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::HarnessError;

/// Canonicalizes `path` while avoiding Windows `\\?\` verbatim prefixes.
#[must_use]
pub fn canonicalize_clean(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Checks whether `path` is a regular file with executable permissions (on Unix).
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Returns the current user's home directory across Windows, macOS, and Linux.
pub fn user_home_dir() -> Result<PathBuf, HarnessError> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    Err(HarnessError::NoHomeDirectory)
}

/// Resolves an explicit project directory or falls back to `current_dir()`.
pub fn resolve_base_dir(explicit: Option<&Path>) -> Result<PathBuf, HarnessError> {
    let raw = match explicit {
        Some(dir) => std::path::absolute(dir).map_err(|source| HarnessError::Io {
            path: dir.to_path_buf(),
            source,
        })?,
        None => std::env::current_dir().map_err(|source| HarnessError::Io {
            path: PathBuf::from("."),
            source,
        })?,
    };
    Ok(canonicalize_clean(&raw))
}

/// Splits `PATH` into directories, expanding `%VAR%` placeholders on Windows.
#[must_use]
pub fn split_expanded_path_var() -> Vec<PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    #[cfg(windows)]
    {
        std::env::split_paths(&path_var)
            .map(|entry| {
                if let Some(raw) = entry.to_str()
                    && raw.contains('%')
                {
                    return PathBuf::from(expand_percent_env_vars(raw));
                }
                entry
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        std::env::split_paths(&path_var).collect()
    }
}

/// Expands `%VAR%` tokens using process environment variables.
#[must_use]
pub fn expand_percent_env_vars(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some((before, after_first)) = rest.split_once('%') {
        out.push_str(before);
        if let Some((var_name, after_second)) = after_first.split_once('%') {
            if var_name.is_empty() {
                out.push_str("%%");
                rest = after_second;
                continue;
            }
            if !var_name.contains([' ', '\t', '/', '\\'])
                && let Ok(val) = std::env::var(var_name)
            {
                out.push_str(&val);
                rest = after_second;
                continue;
            }
            out.push('%');
            rest = after_first;
        } else {
            out.push('%');
            out.push_str(after_first);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Ensures `127.0.0.1`, `localhost`, and `::1` are present in a comma-separated `NO_PROXY` value.
#[must_use]
pub fn merge_loopback_no_proxy(existing: Option<&str>) -> String {
    let mut entries: Vec<String> = existing
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for required in ["127.0.0.1", "localhost", "::1"] {
        if !entries.iter().any(|e| e.eq_ignore_ascii_case(required)) {
            entries.push(required.to_owned());
        }
    }
    entries.join(",")
}

struct UnlinkOnDrop<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for UnlinkOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// Atomically writes `bytes` to `target` via a sibling temporary file, `sync_all`, and `rename`.
pub fn write_file_atomic(
    target: &Path,
    bytes: &[u8],
    secret_mode: bool,
) -> Result<(), HarnessError> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| HarnessError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state");
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4().simple()));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if secret_mode {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = secret_mode;

    let mut guard = UnlinkOnDrop {
        path: &tmp_path,
        armed: true,
    };

    let mut file = opts.open(&tmp_path).map_err(|source| HarnessError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| HarnessError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| HarnessError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    drop(file);

    fs::rename(&tmp_path, target).map_err(|source| HarnessError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    guard.armed = false;
    Ok(())
}

/// Creates a one-time `<path>.liftgate.bak` backup before Liftgate mutates an existing config file.
pub fn backup_once_if_exists(path: &Path) -> Result<(), HarnessError> {
    if !path.is_file() {
        return Ok(());
    }
    let mut bak_os = path.as_os_str().to_os_string();
    bak_os.push(".liftgate.bak");
    let bak_path = PathBuf::from(bak_os);
    if !bak_path.exists() {
        let bytes = fs::read(path).map_err(|source| HarnessError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        write_file_atomic(&bak_path, &bytes, true)?;
    }
    Ok(())
}

/// Reads an optional UTF-8 file, returning `(false, "")` on `NotFound`.
pub fn read_optional_utf8_file(path: &Path) -> Result<(bool, String), HarnessError> {
    match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|source| HarnessError::Utf8 {
                path: path.to_path_buf(),
                source,
            })?;
            Ok((true, text))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((false, String::new())),
        Err(source) => Err(HarnessError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads an existing JSON object file, returning an empty map on `NotFound` or whitespace-only files.
pub fn read_existing_json_object(path: &Path) -> Result<Map<String, Value>, HarnessError> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(HarnessError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let val: Value = serde_json::from_slice(&raw).map_err(|source| HarnessError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    match val {
        Value::Object(map) => Ok(map),
        _ => Err(HarnessError::ConfigRootNotObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Removes `target` from `map` while preserving `preserve_order` sibling entry order.
pub fn remove_key_preserving_order(map: &mut Map<String, Value>, target: &str) -> Option<Value> {
    if !map.contains_key(target) {
        return None;
    }
    let old = std::mem::take(map);
    let mut removed = None;
    for (k, v) in old {
        if k == target {
            removed = Some(v);
        } else {
            map.insert(k, v);
        }
    }
    removed
}

/// Strips JSONC `//` line comments, `/* ... */` block comments, and trailing commas outside string
/// literals while preserving line breaks for accurate error line reporting.
#[must_use]
pub fn strip_jsonc_to_json(input: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CommentState {
        Normal,
        InString { escaped: bool },
        LineComment,
        BlockComment,
    }

    let trimmed_bom = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut no_comments = String::with_capacity(trimmed_bom.len());
    let mut chars = trimmed_bom.chars().peekable();
    let mut state = CommentState::Normal;

    while let Some(ch) = chars.next() {
        match state {
            CommentState::Normal => {
                if ch == '"' {
                    no_comments.push(ch);
                    state = CommentState::InString { escaped: false };
                } else if ch == '/' && chars.peek() == Some(&'/') {
                    let _ = chars.next();
                    state = CommentState::LineComment;
                } else if ch == '/' && chars.peek() == Some(&'*') {
                    let _ = chars.next();
                    state = CommentState::BlockComment;
                } else {
                    no_comments.push(ch);
                }
            }
            CommentState::InString { escaped } => {
                no_comments.push(ch);
                if escaped {
                    state = CommentState::InString { escaped: false };
                } else if ch == '\\' {
                    state = CommentState::InString { escaped: true };
                } else if ch == '"' {
                    state = CommentState::Normal;
                }
            }
            CommentState::LineComment => {
                if ch == '\n' {
                    no_comments.push('\n');
                    state = CommentState::Normal;
                } else if ch == '\r' {
                    no_comments.push('\r');
                    if chars.peek() != Some(&'\n') {
                        state = CommentState::Normal;
                    }
                }
            }
            CommentState::BlockComment => {
                if ch == '*' && chars.peek() == Some(&'/') {
                    let _ = chars.next();
                    no_comments.push(' ');
                    state = CommentState::Normal;
                } else if ch == '\n' || ch == '\r' {
                    no_comments.push(ch);
                }
            }
        }
    }
    if state == CommentState::BlockComment {
        no_comments.push_str("/*");
    }

    let mut out = String::with_capacity(no_comments.len());
    let mut p2 = no_comments.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = p2.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }
        if ch == ',' {
            let mut lookahead = p2.clone();
            let next_sig = lookahead.find(|c| !c.is_whitespace());
            if matches!(next_sig, Some('}' | ']')) {
                continue;
            }
        }
        out.push(ch);
    }
    out
}
