//! Reusable headless control plane for Liftgate (`liftgate` / `lg` CLI and future clients).
//!
//! Centralizes state management, gateway HTTP/SSE telemetry client, embedded web dashboard
//! assets, background daemon supervision, Antigravity CLI (`agy`) installer/updater/auth/proxy
//! lifecycle, persistent Claude/Codex config sync, zero-side-effect CLI/GUI launchers,
//! environment diagnostics, and SHA-256 verified self-upgrade.

pub mod agy_manager;
pub mod auto_update;
pub mod cli;
pub mod client;
pub mod config_sync;
pub mod daemon;
pub mod desktop;
pub mod error;
pub mod format;
pub mod harness;
pub mod serve;
pub mod state;
pub mod terminal;
pub mod upgrade;

/// Compile-time target triple injected by `build.rs`.
pub const BUILD_TARGET: &str = env!("LIFTGATE_BUILD_TARGET");

/// Semantic tone shared across structured diagnostic and status reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportTone {
    /// Healthy settled state.
    Ok,
    /// Active informational state.
    Info,
    /// Recommended next action.
    Action,
    /// Non-fatal caution or state transition.
    Warn,
    /// Error or offline failure.
    Err,
    /// Low-emphasis optional/disabled state.
    Muted,
}
