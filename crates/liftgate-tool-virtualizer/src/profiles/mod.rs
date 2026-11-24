//! Built-in zero-cost [`crate::HarnessToolProfile`] implementations.

mod claude_code;
mod codex;
mod generic;
mod omp;
mod opencode;

pub use claude_code::ClaudeCodeToolProfile;
pub use codex::CodexToolProfile;
pub use generic::GenericToolProfile;
pub use omp::OmpToolProfile;
pub use opencode::OpenCodeToolProfile;
