//! Per-model token pricing (USD per 1M tokens) and the default price list Liftgate ships.
//!
//! Providers publish one price per model family, so a catalog id such as `gemini-4-carbon-high`
//! is priced under its family (`gemini-4-carbon`) by dropping the reasoning-effort suffix, and
//! the Antigravity orchestrator ids are priced as the Gemini 4 model they run on. Defaults carry
//! announced price changes with the instant they take effect, so usage recorded on either side
//! of a change is priced correctly.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::acp::AcpUsage;

/// Date the default price list was checked against the providers' published API prices.
pub const DEFAULT_PRICING_AS_OF: &str = "2026-11-27";

/// Largest accepted price in USD per 1M tokens; anything above is almost certainly a typo.
pub const MAX_PRICE_PER_MTOK: f64 = 10_000.0;

/// Longest accepted pricing key (a model family or model id).
pub const MAX_PRICING_KEY_BYTES: usize = 128;

/// Reasoning-effort suffixes that share their base model's price.
const EFFORT_SUFFIXES: &[&str] = &["-minimal", "-low", "-medium", "-high", "-xhigh", "-max"];

/// Prices in USD per 1M tokens (equivalently, micro-dollars per token).
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelPrice {
    /// Input not served from the prompt cache.
    pub input: f64,
    /// Input served from the prompt cache.
    pub cached_input: f64,
    /// Input written to the prompt cache; equal to `input` for providers that bill implicit
    /// caching as plain input.
    pub cache_write: f64,
    /// Output, including thinking tokens.
    pub output: f64,
}

/// A price outside `0..=MAX_PRICE_PER_MTOK` or not a finite number.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("`{field}` must be a finite price between 0 and 10000 USD per 1M tokens")]
pub struct InvalidPrice {
    pub field: &'static str,
}

impl ModelPrice {
    /// Rejects negative, non-finite, or implausibly large prices.
    ///
    /// # Errors
    ///
    /// Returns the first offending field.
    pub fn validate(&self) -> Result<(), InvalidPrice> {
        for (field, value) in [
            ("input", self.input),
            ("cached_input", self.cached_input),
            ("cache_write", self.cache_write),
            ("output", self.output),
        ] {
            if !value.is_finite() || !(0.0..=MAX_PRICE_PER_MTOK).contains(&value) {
                return Err(InvalidPrice { field });
            }
        }
        Ok(())
    }

    /// What serving `tokens.cached_input` from the cache saved versus billing it as uncached
    /// input, in USD.
    #[must_use]
    pub fn cache_savings_usd(&self, tokens: &UsageTokens) -> f64 {
        tokens_f64(tokens.cached_input) * (self.input - self.cached_input).max(0.0) / 1_000_000.0
    }

    /// Cost of `tokens` in USD.
    #[must_use]
    pub fn cost_usd(&self, tokens: &UsageTokens) -> f64 {
        let micro_usd = tokens_f64(tokens.input) * self.input
            + tokens_f64(tokens.cached_input) * self.cached_input
            + tokens_f64(tokens.cache_write) * self.cache_write
            + tokens_f64(tokens.output) * self.output;
        micro_usd / 1_000_000.0
    }
}

/// Token count as `f64`; exact, since counts never approach 2^53.
const fn tokens_f64(tokens: u64) -> f64 {
    tokens as f64
}

/// Token counts split the way providers bill them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct UsageTokens {
    /// Input not served from the prompt cache.
    #[serde(default)]
    pub input: u64,
    /// Input served from the prompt cache.
    #[serde(default)]
    pub cached_input: u64,
    /// Input written to the prompt cache.
    #[serde(default)]
    pub cache_write: u64,
    /// Output, including thinking.
    #[serde(default)]
    pub output: u64,
    /// Thinking share of `output`.
    #[serde(default)]
    pub thought: u64,
}

impl UsageTokens {
    /// Splits an `agy acp` usage report (whose `output_tokens` already include thinking).
    #[must_use]
    pub const fn from_acp(usage: &AcpUsage) -> Self {
        Self {
            input: usage.input_tokens,
            cached_input: usage.cached_read_tokens,
            cache_write: usage.cached_write_tokens,
            output: usage.output_tokens,
            thought: if usage.thought_tokens < usage.output_tokens {
                usage.thought_tokens
            } else {
                usage.output_tokens
            },
        }
    }

    /// Billable tokens (thinking is part of `output`).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cached_input)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }

    #[must_use]
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self {
            input: self.input.saturating_add(rhs.input),
            cached_input: self.cached_input.saturating_add(rhs.cached_input),
            cache_write: self.cache_write.saturating_add(rhs.cache_write),
            output: self.output.saturating_add(rhs.output),
            thought: self.thought.saturating_add(rhs.thought),
        }
    }
}

/// A default price and the instant it takes effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ScheduledPrice {
    /// Unix milliseconds from which `price` applies (`0` for the opening price).
    pub effective_from_unix_ms: u64,
    pub price: ModelPrice,
}

/// A model family with a published API price.
#[derive(Clone, Copy, Debug)]
pub struct PricingFamily {
    /// Pricing key: the model id without its reasoning-effort suffix.
    pub key: &'static str,
    /// Display name.
    pub label: &'static str,
    /// Exact model ids priced as this family besides `key` and `key-<effort>`.
    pub aliases: &'static [&'static str],
    /// Prices in force over time, ascending; the first one starts at `0`.
    pub schedule: &'static [ScheduledPrice],
    /// Where the prices were published.
    pub source_url: &'static str,
}

impl PricingFamily {
    /// The schedule entry in force at `unix_ms`.
    fn entry_at(&self, unix_ms: u64) -> Option<&'static ScheduledPrice> {
        self.schedule
            .iter()
            .rev()
            .find(|entry| entry.effective_from_unix_ms <= unix_ms)
            .or_else(|| self.schedule.first())
    }

    /// Default price in force at `unix_ms`.
    #[must_use]
    pub fn price_at(&self, unix_ms: u64) -> Option<ModelPrice> {
        self.entry_at(unix_ms).map(|entry| entry.price)
    }

    /// Start of the price period containing `unix_ms`; usage is bucketed by it so later
    /// price changes never re-price earlier usage.
    #[must_use]
    pub fn period_start(&self, unix_ms: u64) -> u64 {
        self.entry_at(unix_ms)
            .map_or(0, |entry| entry.effective_from_unix_ms)
    }

    /// Announced price changes after `unix_ms`.
    pub fn upcoming(&self, unix_ms: u64) -> impl Iterator<Item = ScheduledPrice> + '_ {
        self.schedule
            .iter()
            .filter(move |entry| entry.effective_from_unix_ms > unix_ms)
            .copied()
    }

    /// `true` when `model_id` is this family's key, its key with a reasoning-effort suffix, or
    /// one of its aliases.
    #[must_use]
    pub fn covers(&self, model_id: &str) -> bool {
        self.aliases.contains(&model_id)
            || model_id == self.key
            || model_id
                .strip_prefix(self.key)
                .is_some_and(|rest| EFFORT_SUFFIXES.contains(&rest))
    }
}

const fn price(input: f64, cached_input: f64, cache_write: f64, output: f64) -> ModelPrice {
    ModelPrice {
        input,
        cached_input,
        cache_write,
        output,
    }
}

const fn from_start(price: ModelPrice) -> ScheduledPrice {
    ScheduledPrice {
        effective_from_unix_ms: 0,
        price,
    }
}

/// 2026-12-01 00:00 Pacific (08:00 UTC), when Gemini 4 Carbon and Argon introductory prices end.
const GEMINI_4_STANDARD_PRICING_FROM: u64 = 1_796_112_000_000;
/// 2027-01-01 00:00 Pacific (08:00 UTC), when Gemini 3.8 Flash introductory prices end.
const GEMINI_3_8_FLASH_STANDARD_PRICING_FROM: u64 = 1_798_790_400_000;

const GEMINI_PRICING_URL: &str = "https://ai.google.dev/gemini-api/docs/pricing";
const CLAUDE_PRICING_URL: &str = "https://docs.anthropic.com/en/docs/about-claude/pricing";
const OPENAI_PRICING_URL: &str = "https://openai.com/api/pricing";
const VERTEX_PARTNER_PRICING_URL: &str =
    "https://cloud.google.com/model-garden/partner-models/pricing";

/// Published standard-tier API prices, checked on [`DEFAULT_PRICING_AS_OF`].
///
/// Gemini bills implicit cache writes as plain input; Claude bills 5-minute cache writes at
/// 1.25x input; `gpt-oss-120b` has no first-party API, so it uses Vertex AI's managed price,
/// which lists no cache discount.
pub const DEFAULT_PRICING: &[PricingFamily] = &[
    PricingFamily {
        key: "gemini-4-neon",
        label: "Gemini 4 Neon",
        aliases: &["gemini-pro-orchestrator-neon"],
        schedule: &[from_start(price(3.00, 0.15, 3.00, 15.00))],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-4-carbon",
        label: "Gemini 4 Carbon",
        aliases: &["gemini-pro-orchestrator"],
        schedule: &[
            from_start(price(2.00, 0.10, 2.00, 10.00)),
            ScheduledPrice {
                effective_from_unix_ms: GEMINI_4_STANDARD_PRICING_FROM,
                price: price(4.00, 0.20, 4.00, 20.00),
            },
        ],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-4-argon",
        label: "Gemini 4 Argon",
        aliases: &["gemini-pro-orchestrator-argon"],
        schedule: &[
            from_start(price(2.00, 0.10, 2.00, 10.00)),
            ScheduledPrice {
                effective_from_unix_ms: GEMINI_4_STANDARD_PRICING_FROM,
                price: price(4.00, 0.20, 4.00, 20.00),
            },
        ],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-4-hydrogen",
        label: "Gemini 4 Hydrogen",
        aliases: &[],
        schedule: &[from_start(price(1.50, 0.075, 1.50, 7.50))],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-3.8-flash",
        label: "Gemini 3.8 Flash",
        aliases: &[],
        schedule: &[
            from_start(price(0.75, 0.075, 0.75, 3.75)),
            ScheduledPrice {
                effective_from_unix_ms: GEMINI_3_8_FLASH_STANDARD_PRICING_FROM,
                price: price(1.50, 0.15, 1.50, 7.50),
            },
        ],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "claude-opus-5-5",
        label: "Claude Opus 5.5",
        aliases: &[],
        schedule: &[from_start(price(4.00, 0.20, 5.00, 20.00))],
        source_url: CLAUDE_PRICING_URL,
    },
    PricingFamily {
        key: "claude-sonnet-5-5",
        label: "Claude Sonnet 5.5",
        aliases: &[],
        schedule: &[from_start(price(2.00, 0.10, 2.50, 10.00))],
        source_url: CLAUDE_PRICING_URL,
    },
    PricingFamily {
        key: "gpt-oss-120b",
        label: "GPT-OSS 120B",
        aliases: &[],
        schedule: &[from_start(price(0.09, 0.09, 0.09, 0.36))],
        source_url: VERTEX_PARTNER_PRICING_URL,
    },
    PricingFamily {
        key: "gpt-5.4",
        label: "GPT-5.4",
        aliases: &["gpt-5.3-codex", "gpt-5"],
        schedule: &[from_start(price(2.50, 0.25, 2.50, 15.00))],
        source_url: OPENAI_PRICING_URL,
    },
    PricingFamily {
        key: "gpt-5-mini",
        label: "GPT-5 Mini",
        aliases: &["o4-mini"],
        schedule: &[from_start(price(0.25, 0.025, 0.25, 2.00))],
        source_url: OPENAI_PRICING_URL,
    },
    PricingFamily {
        key: "claude-opus-4-6",
        label: "Claude Opus 4.6",
        aliases: &["claude-opus-4-5"],
        schedule: &[from_start(price(5.00, 0.50, 6.25, 25.00))],
        source_url: CLAUDE_PRICING_URL,
    },
    PricingFamily {
        key: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
        aliases: &["claude-sonnet-4-5"],
        schedule: &[from_start(price(3.00, 0.30, 3.75, 15.00))],
        source_url: CLAUDE_PRICING_URL,
    },
    PricingFamily {
        key: "claude-haiku-4-5",
        label: "Claude Haiku 4.5",
        aliases: &[],
        schedule: &[from_start(price(1.00, 0.10, 1.25, 5.00))],
        source_url: CLAUDE_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-2.5-pro",
        label: "Gemini 2.5 Pro",
        aliases: &[],
        schedule: &[from_start(price(1.25, 0.125, 1.25, 10.00))],
        source_url: GEMINI_PRICING_URL,
    },
    PricingFamily {
        key: "gemini-2.5-flash",
        label: "Gemini 2.5 Flash",
        aliases: &["gemini-2.5-flash-lite"],
        schedule: &[from_start(price(0.30, 0.03, 0.30, 2.50))],
        source_url: GEMINI_PRICING_URL,
    },
];

/// Default family pricing `model_id`, if Liftgate ships a price for it.
#[must_use]
pub fn family_for_model(model_id: &str) -> Option<&'static PricingFamily> {
    let id = model_id.trim();
    DEFAULT_PRICING.iter().find(|family| family.covers(id))
}

/// Default family with pricing key `key`.
#[must_use]
pub fn default_family(key: &str) -> Option<&'static PricingFamily> {
    DEFAULT_PRICING.iter().find(|family| family.key == key)
}

/// Key `model_id` is priced under: its default family, else the id without a reasoning-effort
/// suffix (so every effort of an unknown model shares one custom price).
#[must_use]
pub fn pricing_key(model_id: &str) -> &str {
    let id = model_id.trim();
    if let Some(family) = family_for_model(id) {
        return family.key;
    }
    EFFORT_SUFFIXES
        .iter()
        .find_map(|suffix| id.strip_suffix(suffix).filter(|base| !base.is_empty()))
        .unwrap_or(id)
}

/// One priced (or priceable) model family in `GET /api/telemetry/pricing`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelPricingEntry {
    pub key: String,
    /// Display name of a default family.
    #[serde(default)]
    pub label: Option<String>,
    /// Catalog and recorded model ids priced under `key`.
    #[serde(default)]
    pub model_ids: Vec<String>,
    /// Liftgate's default price in force now (`None` for models it ships no price for).
    #[serde(default)]
    pub default_price: Option<ModelPrice>,
    /// Announced default price changes still ahead.
    #[serde(default)]
    pub upcoming_defaults: Vec<ScheduledPrice>,
    /// The user's price, which replaces the default for all recorded and future usage.
    #[serde(default)]
    pub custom_price: Option<ModelPrice>,
    #[serde(default)]
    pub source_url: Option<String>,
}

/// Response served by `GET /api/telemetry/pricing` and its update endpoint.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PricingResponse {
    #[serde(default)]
    pub revision: u64,
    /// Always `USD`; prices are per 1M tokens.
    #[serde(default)]
    pub currency: String,
    /// When the shipped defaults were last checked.
    #[serde(default)]
    pub defaults_as_of: String,
    #[serde(default)]
    pub entries: Vec<ModelPricingEntry>,
}

/// Recorded usage of one model.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelUsage {
    pub model_id: String,
    #[serde(default)]
    pub pricing_key: String,
    /// Completed requests.
    #[serde(default)]
    pub calls: u64,
    /// Of `calls`, auxiliary requests (titles, summaries, classifiers).
    #[serde(default)]
    pub auxiliary_calls: u64,
    #[serde(default)]
    pub tokens: UsageTokens,
    /// `None` when the model has neither a default nor a custom price.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub first_used_at_unix_ms: u64,
    #[serde(default)]
    pub last_used_at_unix_ms: u64,
}

/// Recorded token usage and USD cost attributed to one upstream provider.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub auxiliary_calls: u64,
    #[serde(default)]
    pub tokens: UsageTokens,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub cache_savings_usd: f64,
    #[serde(default)]
    pub unpriced_calls: u64,
}

/// Usage totals carried on the snapshot as `usage` (everything recorded) and served for a time
/// range by `GET /api/telemetry/usage?from=&to=`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct UsageSummary {
    /// Bumped whenever usage or prices change.
    #[serde(default)]
    pub revision: u64,
    /// Start of the covered range: when recording started (or was last reset) for the all-time
    /// totals, the requested `from` otherwise.
    #[serde(default)]
    pub since_unix_ms: u64,
    /// End of the requested range; absent for the all-time totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_unix_ms: Option<u64>,
    /// Where bucketed history starts. Older usage survives only in the all-time totals, so
    /// ranges reaching further back are partial.
    #[serde(default)]
    pub history_from_unix_ms: u64,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub tokens: UsageTokens,
    /// Cost of priced usage.
    #[serde(default)]
    pub cost_usd: f64,
    /// What cache hits saved versus uncached input, priced per price period like `cost_usd`.
    #[serde(default)]
    pub cache_savings_usd: f64,
    /// Calls on models without a price, left out of `cost_usd`.
    #[serde(default)]
    pub unpriced_calls: u64,
    /// Per-provider breakdown across the same covered range.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_provider: Vec<ProviderUsage>,
}

/// Usage within one interval of `GET /api/telemetry/usage/series`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct UsageInterval {
    pub start_unix_ms: u64,
    pub end_unix_ms: u64,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub auxiliary_calls: u64,
    #[serde(default)]
    pub tokens: UsageTokens,
    /// Cost of priced usage in the interval.
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub cache_savings_usd: f64,
    /// Calls on models without a price, left out of `cost_usd`.
    #[serde(default)]
    pub unpriced_calls: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_provider: Vec<ProviderUsage>,
}

/// Response served by `GET /api/telemetry/usage/series?bounds=<t0,t1,…>`: one interval per
/// consecutive pair of bounds. Usage is kept in [`Self::bucket_ms`] buckets counted toward the
/// interval their start falls in, so bounds on the caller's local midnights yield its local days.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct UsageSeriesResponse {
    #[serde(default)]
    pub revision: u64,
    /// Width of the usage buckets intervals are built from.
    #[serde(default)]
    pub bucket_ms: u64,
    /// Where bucketed history starts; intervals before it read as zero.
    #[serde(default)]
    pub history_from_unix_ms: u64,
    #[serde(default)]
    pub intervals: Vec<UsageInterval>,
}

/// Response served by `GET /api/telemetry/usage`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub summary: UsageSummary,
    /// Per-model usage, costliest first.
    #[serde(default)]
    pub models: Vec<ModelUsage>,
    /// Per-provider usage, costliest first.
    #[serde(default)]
    pub providers: Vec<ProviderUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_map_to_their_published_family() {
        for (id, key) in [
            ("gemini-4-carbon-high", "gemini-4-carbon"),
            ("gemini-4-carbon", "gemini-4-carbon"),
            ("gemini-pro-orchestrator", "gemini-4-carbon"),
            ("gemini-pro-orchestrator-neon", "gemini-4-neon"),
            ("gemini-pro-orchestrator-argon", "gemini-4-argon"),
            ("gemini-4-neon-xhigh", "gemini-4-neon"),
            ("gemini-4-hydrogen-low", "gemini-4-hydrogen"),
            ("gemini-3.8-flash-medium", "gemini-3.8-flash"),
            ("claude-opus-5-5-high", "claude-opus-5-5"),
            ("claude-sonnet-5-5-low", "claude-sonnet-5-5"),
            ("gpt-oss-120b-medium", "gpt-oss-120b"),
        ] {
            assert_eq!(pricing_key(id), key, "{id}");
            assert!(family_for_model(id).is_some(), "{id}");
        }
        // Unknown models share one key across efforts but have no default price.
        assert_eq!(pricing_key("gemini-5-quark-high"), "gemini-5-quark");
        assert_eq!(pricing_key("my-model"), "my-model");
        assert!(family_for_model("gemini-4-carbon-lite-high").is_none());
        assert!(family_for_model("gemini-5-quark-high").is_none());
    }

    #[test]
    fn scheduled_defaults_switch_at_their_effective_instant() {
        let carbon = default_family("gemini-4-carbon").expect("carbon");
        let before = GEMINI_4_STANDARD_PRICING_FROM - 1;
        assert_eq!(carbon.price_at(before).map(|p| p.input), Some(2.00));
        assert_eq!(carbon.period_start(before), 0);
        assert_eq!(
            carbon
                .price_at(GEMINI_4_STANDARD_PRICING_FROM)
                .map(|p| p.output),
            Some(20.00)
        );
        assert_eq!(
            carbon.period_start(GEMINI_4_STANDARD_PRICING_FROM + 5),
            GEMINI_4_STANDARD_PRICING_FROM
        );
        assert_eq!(carbon.upcoming(before).count(), 1);
        assert_eq!(carbon.upcoming(GEMINI_4_STANDARD_PRICING_FROM).count(), 0);
        let neon = default_family("gemini-4-neon").expect("neon");
        assert_eq!(neon.period_start(u64::MAX), 0);
        for family in DEFAULT_PRICING {
            assert_eq!(
                family.schedule.first().map(|e| e.effective_from_unix_ms),
                Some(0)
            );
            assert!(family.schedule.windows(2).all(
                |w| matches!(w, [a, b] if a.effective_from_unix_ms < b.effective_from_unix_ms)
            ));
            for entry in family.schedule {
                assert_eq!(entry.price.validate(), Ok(()), "{}", family.key);
            }
        }
    }

    #[test]
    fn cost_bills_each_token_class_at_its_price() {
        let tokens = UsageTokens::from_acp(&AcpUsage {
            input_tokens: 1_000_000,
            output_tokens: 200_000,
            cached_read_tokens: 4_000_000,
            cached_write_tokens: 100_000,
            thought_tokens: 500_000,
            total_tokens: 0,
        });
        assert_eq!(tokens.thought, 200_000, "thinking is a share of output");
        assert_eq!(tokens.total(), 5_300_000);
        let sonnet = price(2.00, 0.10, 2.50, 10.00);
        let cost = sonnet.cost_usd(&tokens);
        assert!((cost - (2.00 + 0.40 + 0.25 + 2.00)).abs() < 1e-9, "{cost}");
        assert_eq!(ModelPrice::default().cost_usd(&tokens), 0.0);
        // 4M cached tokens at $2.00 instead of $0.10.
        assert!((sonnet.cache_savings_usd(&tokens) - 7.60).abs() < 1e-9);
        assert_eq!(
            price(0.09, 0.09, 0.09, 0.36).cache_savings_usd(&tokens),
            0.0
        );
    }

    #[test]
    fn invalid_prices_are_rejected() {
        let mut p = price(1.0, 0.1, 1.0, 5.0);
        assert_eq!(p.validate(), Ok(()));
        p.output = -1.0;
        assert_eq!(p.validate(), Err(InvalidPrice { field: "output" }));
        p.output = f64::NAN;
        assert!(p.validate().is_err());
        p.output = MAX_PRICE_PER_MTOK + 1.0;
        assert!(p.validate().is_err());
    }
}
