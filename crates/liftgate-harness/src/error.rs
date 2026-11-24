//! Typed error definitions for harness configuration editors and executable discovery.

use std::path::PathBuf;

use thiserror::Error;

/// Structured error returned by harness configuration sync, ledger I/O, and binary resolution.
#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(
        "could not determine user home directory; set `LIFTGATE_HOME` or `HOME` / `USERPROFILE`"
    )]
    NoHomeDirectory,

    #[error("filesystem operation failed at `{}`", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid JSON file at `{}`", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("file `{}` is not valid UTF-8", path.display())]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error(
        "configuration file `{}` must contain a top-level JSON object (`{{ ... }}`)",
        path.display()
    )]
    ConfigRootNotObject { path: PathBuf },

    #[error("invalid JSON/JSONC in environment variable `{var}`")]
    InvalidEnvConfigJson {
        var: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("environment variable `{var}` must evaluate to a top-level JSON object (`{{ ... }}`)")]
    EnvConfigNotObject { var: &'static str },

    #[error(
        "unsupported or malformed YAML layout in `{}`: {detail}",
        path.display()
    )]
    UnsupportedYamlLayout { path: PathBuf, detail: String },

    #[error("`{tool}` CLI executable was not found ({hint})")]
    HarnessNotFound {
        tool: &'static str,
        hint: &'static str,
    },
}
