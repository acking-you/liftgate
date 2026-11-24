//! Shared CLI flag slice helpers across harness adapters.

/// Returns the prefix of `args` preceding a standalone `--` argument separator.
#[must_use]
pub fn slice_before_double_dash(args: &[String]) -> &[String] {
    match args.iter().position(|a| a == "--") {
        Some(pos) => args.get(..pos).unwrap_or_default(),
        None => args,
    }
}

/// Extracts an explicit `-m <model>`, `--model <model>`, `-m=<model>`, or `--model=<model>`
/// argument passed before `--`.
#[must_use]
pub fn extract_explicit_model_arg(args: &[String]) -> Option<&str> {
    let pre_dd = slice_before_double_dash(args);
    let mut idx = 0usize;
    while let Some(tok) = pre_dd.get(idx) {
        let s = tok.as_str();
        if matches!(s, "-m" | "--model") {
            return pre_dd
                .get(idx + 1)
                .map(String::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty());
        }
        if let Some(val) = s.strip_prefix("--model=").or_else(|| s.strip_prefix("-m=")) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        idx += 1;
    }
    None
}
