use crate::acp::{AcpUsage, AvailableModel};

/// Shared test catalog of Antigravity ACP models for downstream unit and integration tests.
#[must_use]
pub fn mock_available_models() -> Vec<AvailableModel> {
    vec![
        AvailableModel {
            model_id: "gemini-4-hydrogen-low".to_owned(),
            name: "Gemini 4 Hydrogen (Low)".to_owned(),
        },
        AvailableModel {
            model_id: "gemini-4-hydrogen-xhigh".to_owned(),
            name: "Gemini 4 Hydrogen (XHigh)".to_owned(),
        },
        AvailableModel {
            model_id: "gemini-4-carbon-high".to_owned(),
            name: "Gemini 4 Carbon (High)".to_owned(),
        },
        AvailableModel {
            model_id: "gemini-4-carbon-medium".to_owned(),
            name: "Gemini 4 Carbon (Medium)".to_owned(),
        },
        AvailableModel {
            model_id: "gemini-3.8-flash-high".to_owned(),
            name: "Gemini 3.8 Flash (High)".to_owned(),
        },
        AvailableModel {
            model_id: "gemini-3.8-flash-low".to_owned(),
            name: "Gemini 3.8 Flash (Low)".to_owned(),
        },
        AvailableModel {
            model_id: "claude-sonnet-5-5-high".to_owned(),
            name: "Claude Sonnet 5.5 (High)".to_owned(),
        },
        AvailableModel {
            model_id: "claude-opus-5-5-high".to_owned(),
            name: "Claude Opus 5.5 (High)".to_owned(),
        },
    ]
}

#[must_use]
pub const fn mock_acp_usage(input: u64, output: u64, thought: u64) -> AcpUsage {
    AcpUsage {
        input_tokens: input,
        output_tokens: output,
        cached_read_tokens: 128,
        cached_write_tokens: 0,
        thought_tokens: thought,
        total_tokens: input + output + 128,
    }
}
