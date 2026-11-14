//! Pure wire, configuration, and protocol translation types for `liftgate`.

pub mod acp;
pub mod anthropic;
pub mod canonical;
pub mod config;
pub mod error;
pub mod model;
pub mod openai;
pub mod parser;
pub mod session_key;
pub mod telemetry;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
