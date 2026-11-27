//! Async ACP process supervisor, bounded session pool, and HTTP gateway server.

pub mod acp_client;
pub mod context_capture;
pub mod error;
pub mod job;
pub mod mcp_bridge;
pub mod platform;
pub mod routes;
pub mod session_pool;
pub mod spill_store;
pub mod telemetry;
pub mod title_model;
pub mod tool_calls;
pub mod tool_catalog;
pub mod turn_runner;
pub mod usage_ledger;
pub mod web_fetch;
