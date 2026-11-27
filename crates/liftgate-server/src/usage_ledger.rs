//! Per-model token usage ledger and the price book that turns it into USD.
//!
//! Every completed request adds its token usage to a row keyed by model and default price
//! period, so an announced price change never re-prices usage recorded before it. Costs are
//! derived on read from the price book: custom prices (persisted in `model-pricing.json`)
//! replace the shipped defaults for all recorded usage, which keeps every total consistent with
//! the prices the dashboard shows.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use liftgate_types::acp::AvailableModel;
use liftgate_types::pricing::{
    DEFAULT_PRICING, DEFAULT_PRICING_AS_OF, MAX_PRICING_KEY_BYTES, ModelPrice, ModelPricingEntry,
    ModelUsage, PricingResponse, UsageResponse, UsageSummary, UsageTokens, default_family,
    family_for_model, pricing_key,
};
use serde::{Deserialize, Serialize};

/// Most (model, price period) rows kept; the least recently used row is dropped beyond it.
pub const MAX_LEDGER_ROWS: usize = 512;
/// Most custom prices kept.
pub const MAX_CUSTOM_PRICES: usize = 256;
const PRICE_BOOK_SCHEMA_VERSION: u32 = 1;

type TokenField = fn(&mut UsageTokens) -> &mut u64;

/// Every billed token class, for corrections applied class by class.
const TOKEN_FIELDS: [TokenField; 5] = [
    |t| &mut t.input,
    |t| &mut t.cached_input,
    |t| &mut t.cache_write,
    |t| &mut t.output,
    |t| &mut t.thought,
];

/// Usage of one model within one default price period.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedgerRow {
    pub model_id: String,
    /// Start of the default price period the usage falls in (`0` for the opening period).
    #[serde(default)]
    pub period_start_unix_ms: u64,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub auxiliary_calls: u64,
    #[serde(default)]
    pub tokens: UsageTokens,
    #[serde(default)]
    pub first_used_at_unix_ms: u64,
    #[serde(default)]
    pub last_used_at_unix_ms: u64,
}

/// Cumulative token usage per model since `since_unix_ms`.
#[derive(Clone, Debug, Default)]
pub struct UsageLedger {
    rows: Vec<LedgerRow>,
    since_unix_ms: u64,
    revision: u64,
    /// Bumped by [`Self::reset`]; corrections from cascades that began before a reset are dropped.
    epoch: u64,
}

/// Model id cut to [`MAX_PRICING_KEY_BYTES`] on a char boundary.
fn bounded_id(model_id: &str) -> &str {
    let id = model_id.trim();
    id.get(..id.floor_char_boundary(MAX_PRICING_KEY_BYTES))
        .unwrap_or(id)
}

/// Start of the default price period `model_id` is in at `unix_ms`.
fn period_start(model_id: &str, unix_ms: u64) -> u64 {
    family_for_model(model_id).map_or(0, |family| family.period_start(unix_ms))
}

impl UsageLedger {
    /// Restores persisted rows; a ledger without a start time starts now.
    #[must_use]
    pub fn from_persisted(since_unix_ms: u64, mut rows: Vec<LedgerRow>, now_ms: u64) -> Self {
        rows.retain(|row| !row.model_id.trim().is_empty());
        if rows.len() > MAX_LEDGER_ROWS {
            rows.sort_by_key(|row| std::cmp::Reverse(row.last_used_at_unix_ms));
            rows.truncate(MAX_LEDGER_ROWS);
        }
        Self {
            rows,
            since_unix_ms: if since_unix_ms > 0 {
                since_unix_ms
            } else {
                now_ms
            },
            revision: 1,
            epoch: 0,
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[LedgerRow] {
        &self.rows
    }

    #[must_use]
    pub const fn since_unix_ms(&self) -> u64 {
        self.since_unix_ms
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Current reset epoch; capture it when an MCP cascade starts and hand it back to
    /// [`Self::reconcile`].
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    fn row_mut(&mut self, model_id: &str, period: u64, now_ms: u64) -> Option<&mut LedgerRow> {
        let pos = match self
            .rows
            .iter()
            .position(|row| row.model_id == model_id && row.period_start_unix_ms == period)
        {
            Some(pos) => pos,
            None => {
                if self.rows.len() >= MAX_LEDGER_ROWS
                    && let Some(oldest) = self
                        .rows
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, row)| row.last_used_at_unix_ms)
                        .map(|(pos, _)| pos)
                {
                    let dropped = self.rows.swap_remove(oldest);
                    tracing::warn!(
                        model = %dropped.model_id,
                        "usage ledger full; dropped its least recently used row"
                    );
                }
                self.rows.push(LedgerRow {
                    model_id: model_id.to_owned(),
                    period_start_unix_ms: period,
                    first_used_at_unix_ms: now_ms,
                    ..LedgerRow::default()
                });
                self.rows.len().checked_sub(1)?
            }
        };
        self.rows.get_mut(pos)
    }

    /// Adds one completed request.
    pub fn record(&mut self, model_id: &str, auxiliary: bool, tokens: UsageTokens, now_ms: u64) {
        let id = bounded_id(model_id);
        if id.is_empty() {
            return;
        }
        let period = period_start(id, now_ms);
        let Some(row) = self.row_mut(id, period, now_ms) else {
            return;
        };
        row.calls = row.calls.saturating_add(1);
        if auxiliary {
            row.auxiliary_calls = row.auxiliary_calls.saturating_add(1);
        }
        row.tokens = row.tokens.saturating_add(tokens);
        row.last_used_at_unix_ms = now_ms;
        self.revision = self.revision.saturating_add(1);
    }

    /// Replaces step estimates already recorded for `model_id` with the authoritative usage
    /// reported once its MCP cascade finished.
    ///
    /// Growth lands on the newest price period. Shrinkage drains the newest period first and
    /// carries into earlier ones, so a cascade that straddled a price change never loses part of
    /// its correction to saturation and the model's totals stay exact. A cascade that began before
    /// the latest reset (an older `epoch`) is ignored: its estimates were cleared with the rest.
    pub fn reconcile(
        &mut self,
        model_id: &str,
        epoch: u64,
        accounted: UsageTokens,
        authoritative: UsageTokens,
        now_ms: u64,
    ) {
        if epoch != self.epoch {
            return;
        }
        let id = bounded_id(model_id);
        let period = period_start(id, now_ms);
        let mut rows: Vec<&mut LedgerRow> = self
            .rows
            .iter_mut()
            .filter(|row| row.model_id == id && row.period_start_unix_ms <= period)
            .collect();
        if rows.is_empty() {
            return;
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.period_start_unix_ms));
        for field in TOKEN_FIELDS {
            let (mut acc, mut auth) = (accounted, authoritative);
            let (acc, auth) = (*field(&mut acc), *field(&mut auth));
            if auth >= acc {
                if let Some(newest) = rows.first_mut() {
                    let slot = field(&mut newest.tokens);
                    *slot = slot.saturating_add(auth - acc);
                }
                continue;
            }
            let mut excess = acc - auth;
            for row in &mut rows {
                if excess == 0 {
                    break;
                }
                let slot = field(&mut row.tokens);
                let take = excess.min(*slot);
                *slot -= take;
                excess -= take;
            }
        }
        self.revision = self.revision.saturating_add(1);
    }

    /// Drops all recorded usage and starts counting again from `now_ms`.
    pub fn reset(&mut self, now_ms: u64) {
        self.rows.clear();
        self.since_unix_ms = now_ms;
        self.revision = self.revision.saturating_add(1);
        self.epoch = self.epoch.saturating_add(1);
    }

    /// Per-model usage (periods merged), costliest first, plus totals.
    #[must_use]
    pub fn report(&self, prices: &PriceBook) -> UsageResponse {
        let mut by_model: BTreeMap<&str, ModelUsage> = BTreeMap::new();
        let mut cache_savings_usd = 0.0;
        for row in &self.rows {
            let key = pricing_key(&row.model_id);
            let price = prices.price_for(key, row.period_start_unix_ms);
            let cost = price.map(|price| price.cost_usd(&row.tokens));
            if let Some(price) = price {
                cache_savings_usd += price.cache_savings_usd(&row.tokens);
            }
            let entry = by_model
                .entry(row.model_id.as_str())
                .or_insert_with(|| ModelUsage {
                    model_id: row.model_id.clone(),
                    pricing_key: key.to_owned(),
                    cost_usd: Some(0.0),
                    first_used_at_unix_ms: row.first_used_at_unix_ms,
                    ..ModelUsage::default()
                });
            entry.calls = entry.calls.saturating_add(row.calls);
            entry.auxiliary_calls = entry.auxiliary_calls.saturating_add(row.auxiliary_calls);
            entry.tokens = entry.tokens.saturating_add(row.tokens);
            entry.cost_usd = match (entry.cost_usd, cost) {
                (Some(total), Some(cost)) => Some(total + cost),
                _ => None,
            };
            entry.first_used_at_unix_ms =
                entry.first_used_at_unix_ms.min(row.first_used_at_unix_ms);
            entry.last_used_at_unix_ms = entry.last_used_at_unix_ms.max(row.last_used_at_unix_ms);
        }
        let mut models: Vec<ModelUsage> = by_model.into_values().collect();
        models.sort_by(|a, b| {
            b.cost_usd
                .unwrap_or(-1.0)
                .total_cmp(&a.cost_usd.unwrap_or(-1.0))
                .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
        });
        let mut summary = UsageSummary {
            revision: self.revision.saturating_add(prices.revision()),
            since_unix_ms: self.since_unix_ms,
            cache_savings_usd,
            ..UsageSummary::default()
        };
        for model in &models {
            summary.calls = summary.calls.saturating_add(model.calls);
            summary.tokens = summary.tokens.saturating_add(model.tokens);
            match model.cost_usd {
                Some(cost) => summary.cost_usd += cost,
                None => summary.unpriced_calls = summary.unpriced_calls.saturating_add(model.calls),
            }
        }
        UsageResponse { summary, models }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PersistedPriceBook {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    custom_prices: BTreeMap<String, ModelPrice>,
}

/// Result of [`PriceBook::set`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceUpdate {
    Changed,
    Unchanged,
    /// A new custom price was refused because [`MAX_CUSTOM_PRICES`] are already set.
    Full,
}

/// Custom per-model prices layered over the shipped defaults.
#[derive(Debug, Default)]
pub struct PriceBook {
    path: Option<PathBuf>,
    custom: BTreeMap<String, ModelPrice>,
    revision: u64,
    persist_seq: u64,
}

impl PriceBook {
    /// Loads custom prices from `path`, skipping invalid entries.
    #[must_use]
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut custom = BTreeMap::new();
        if let Some(file) = path.as_deref() {
            match fs::read(file) {
                Ok(bytes) => match serde_json::from_slice::<PersistedPriceBook>(&bytes) {
                    Ok(persisted) => {
                        custom = persisted
                            .custom_prices
                            .into_iter()
                            .filter(|(key, price)| {
                                !key.trim().is_empty()
                                    && key.len() <= MAX_PRICING_KEY_BYTES
                                    && price.validate().is_ok()
                            })
                            .take(MAX_CUSTOM_PRICES)
                            .collect();
                    }
                    Err(err) => tracing::warn!(
                        path = %file.display(),
                        error = ?err,
                        "corrupt model pricing file; using default prices"
                    ),
                },
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(
                    path = %file.display(),
                    error = ?err,
                    "failed to read model pricing file; using default prices"
                ),
            }
        }
        Self {
            path,
            custom,
            revision: 1,
            persist_seq: 0,
        }
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Price for `key` at `unix_ms`: the custom price, else the default then in force.
    #[must_use]
    pub fn price_for(&self, key: &str, unix_ms: u64) -> Option<ModelPrice> {
        self.custom
            .get(key)
            .copied()
            .or_else(|| default_family(key).and_then(|family| family.price_at(unix_ms)))
    }

    /// Cost of `tokens` used by `model_id` at `unix_ms`, if the model is priced.
    #[must_use]
    pub fn cost_of(&self, model_id: &str, tokens: &UsageTokens, unix_ms: u64) -> Option<f64> {
        self.price_for(pricing_key(model_id), unix_ms)
            .map(|price| price.cost_usd(tokens))
    }

    /// Sets (`Some`) or removes (`None`) the custom price of `key`.
    pub fn set(&mut self, key: &str, price: Option<ModelPrice>) -> PriceUpdate {
        let changed = match price {
            Some(price) => {
                if !self.custom.contains_key(key) && self.custom.len() >= MAX_CUSTOM_PRICES {
                    return PriceUpdate::Full;
                }
                self.custom.insert(key.to_owned(), price) != Some(price)
            }
            None => self.custom.remove(key).is_some(),
        };
        if !changed {
            return PriceUpdate::Unchanged;
        }
        self.revision = self.revision.saturating_add(1);
        PriceUpdate::Changed
    }

    /// Serializes the book for an atomic write; the sequence orders concurrent writers.
    pub fn prepare_persist(&mut self) -> Option<(PathBuf, u64, Vec<u8>)> {
        let path = self.path.clone()?;
        self.persist_seq = self.persist_seq.saturating_add(1);
        let persisted = PersistedPriceBook {
            version: PRICE_BOOK_SCHEMA_VERSION,
            custom_prices: self.custom.clone(),
        };
        match serde_json::to_vec_pretty(&persisted) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                Some((path, self.persist_seq, bytes))
            }
            Err(err) => {
                tracing::warn!(error = ?err, "failed to serialize model pricing");
                None
            }
        }
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Every shipped family, then every other custom-priced key or key of a catalog or recorded
    /// model, each with the model ids it prices.
    #[must_use]
    pub fn report(
        &self,
        catalog: &[AvailableModel],
        ledger: &UsageLedger,
        now_ms: u64,
    ) -> PricingResponse {
        fn entry_for<'a>(
            defaults: &'a mut [ModelPricingEntry],
            extra: &'a mut BTreeMap<String, ModelPricingEntry>,
            key: &str,
        ) -> &'a mut ModelPricingEntry {
            if let Some(entry) = defaults.iter_mut().find(|entry| entry.key == key) {
                return entry;
            }
            extra
                .entry(key.to_owned())
                .or_insert_with(|| ModelPricingEntry {
                    key: key.to_owned(),
                    ..ModelPricingEntry::default()
                })
        }

        let mut entries: Vec<ModelPricingEntry> = DEFAULT_PRICING
            .iter()
            .map(|family| ModelPricingEntry {
                key: family.key.to_owned(),
                label: Some(family.label.to_owned()),
                default_price: family.price_at(now_ms),
                upcoming_defaults: family.upcoming(now_ms).collect(),
                source_url: Some(family.source_url.to_owned()),
                ..ModelPricingEntry::default()
            })
            .collect();
        let mut extra: BTreeMap<String, ModelPricingEntry> = BTreeMap::new();
        for (key, price) in &self.custom {
            entry_for(&mut entries, &mut extra, key).custom_price = Some(*price);
        }
        let seen = catalog
            .iter()
            .map(|model| model.model_id.as_str())
            .chain(ledger.rows().iter().map(|row| row.model_id.as_str()));
        for id in seen {
            let id = bounded_id(id);
            if id.is_empty() {
                continue;
            }
            let entry = entry_for(&mut entries, &mut extra, pricing_key(id));
            if !entry.model_ids.iter().any(|known| known == id) {
                entry.model_ids.push(id.to_owned());
            }
        }
        entries.extend(extra.into_values());
        for entry in &mut entries {
            entry.model_ids.sort();
        }
        PricingResponse {
            revision: self.revision,
            currency: "USD".to_owned(),
            defaults_as_of: DEFAULT_PRICING_AS_OF.to_owned(),
            entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEC_1_2026: u64 = 1_796_112_000_000;

    fn tokens(input: u64, cached: u64, output: u64) -> UsageTokens {
        UsageTokens {
            input,
            cached_input: cached,
            cache_write: 0,
            output,
            thought: 0,
        }
    }

    fn catalog(ids: &[&str]) -> Vec<AvailableModel> {
        ids.iter()
            .map(|id| AvailableModel {
                model_id: (*id).to_owned(),
                name: (*id).to_owned(),
            })
            .collect()
    }

    #[test]
    fn usage_is_priced_per_period_and_custom_prices_replace_defaults() {
        let mut ledger = UsageLedger::from_persisted(0, Vec::new(), 1_000);
        assert_eq!(ledger.since_unix_ms(), 1_000);
        // 1M uncached input + 1M output on Carbon, once before and once after the Dec 1 change.
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(1_000_000, 0, 1_000_000),
            DEC_1_2026 - 10,
        );
        ledger.record(
            "gemini-4-carbon-low",
            true,
            tokens(1_000_000, 0, 1_000_000),
            DEC_1_2026 + 10,
        );
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(1_000_000, 0, 1_000_000),
            DEC_1_2026 + 20,
        );
        ledger.record(
            "mystery-model-high",
            false,
            tokens(10, 0, 10),
            DEC_1_2026 + 30,
        );
        assert_eq!(ledger.rows().len(), 4, "model x period rows");

        let mut prices = PriceBook::load(None);
        let report = ledger.report(&prices);
        let high = report
            .models
            .iter()
            .find(|m| m.model_id == "gemini-4-carbon-high")
            .expect("high");
        assert_eq!(high.calls, 2);
        assert_eq!(high.pricing_key, "gemini-4-carbon");
        // $2 + $10 before the change, $4 + $20 after.
        assert!((high.cost_usd.unwrap_or_default() - 36.0).abs() < 1e-9);
        let low = report
            .models
            .iter()
            .find(|m| m.model_id == "gemini-4-carbon-low")
            .expect("low");
        assert_eq!(low.auxiliary_calls, 1);
        assert_eq!(report.summary.calls, 4);
        assert_eq!(report.summary.unpriced_calls, 1);
        assert_eq!(
            report.summary.cache_savings_usd, 0.0,
            "nothing was cached yet"
        );
        assert!((report.summary.cost_usd - 60.0).abs() < 1e-9);
        assert_eq!(
            report.models.first().map(|m| m.model_id.as_str()),
            Some("gemini-4-carbon-high")
        );
        assert_eq!(report.models.last().and_then(|m| m.cost_usd), None);

        // A custom price replaces both default periods and prices the unknown model.
        let flat = ModelPrice {
            input: 1.0,
            cached_input: 0.1,
            cache_write: 1.0,
            output: 1.0,
        };
        assert_eq!(
            prices.set("gemini-4-carbon", Some(flat)),
            PriceUpdate::Changed
        );
        assert_eq!(
            prices.set("gemini-4-carbon", Some(flat)),
            PriceUpdate::Unchanged
        );
        assert_eq!(
            prices.set("mystery-model", Some(ModelPrice::default())),
            PriceUpdate::Changed
        );
        let repriced = ledger.report(&prices);
        assert!((repriced.summary.cost_usd - 6.0).abs() < 1e-9);
        assert_eq!(repriced.summary.unpriced_calls, 0);
        assert!(repriced.summary.revision > report.summary.revision);
        assert_eq!(prices.set("mystery-model", None), PriceUpdate::Changed);
        assert_eq!(prices.set("mystery-model", None), PriceUpdate::Unchanged);
        assert_eq!(ledger.report(&prices).summary.unpriced_calls, 1);

        ledger.reset(5_000);
        assert!(ledger.rows().is_empty());
        assert_eq!(ledger.since_unix_ms(), 5_000);
    }

    #[test]
    fn cache_savings_follow_the_price_of_each_period() {
        // 1M cached Carbon tokens on each side of the Dec 1 change: $1.90 then $3.80 saved.
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(0, 1_000_000, 0),
            DEC_1_2026 - 10,
        );
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(0, 1_000_000, 0),
            DEC_1_2026 + 10,
        );
        let mut prices = PriceBook::load(None);
        let saved = ledger.report(&prices).summary.cache_savings_usd;
        assert!((saved - (1.90 + 3.80)).abs() < 1e-9, "{saved}");
        // A custom price applies to all history, savings included.
        let _ = prices.set(
            "gemini-4-carbon",
            Some(ModelPrice {
                input: 1.0,
                cached_input: 0.5,
                cache_write: 1.0,
                output: 1.0,
            }),
        );
        let saved = ledger.report(&prices).summary.cache_savings_usd;
        assert!((saved - 1.0).abs() < 1e-9, "{saved}");
    }

    #[test]
    fn reconcile_corrects_the_models_latest_period() {
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        ledger.record("gemini-4-neon-high", false, tokens(500, 0, 50), 10);
        ledger.reconcile(
            "gemini-4-neon-high",
            ledger.epoch(),
            tokens(500, 0, 50),
            tokens(100, 400, 60),
            20,
        );
        let row = ledger.rows().first().expect("row");
        assert_eq!(row.tokens, tokens(100, 400, 60));
        ledger.reconcile(
            "unknown",
            ledger.epoch(),
            tokens(1, 1, 1),
            tokens(2, 2, 2),
            30,
        );
        assert_eq!(ledger.rows().len(), 1);
    }

    #[test]
    fn corrections_from_before_a_reset_are_dropped() {
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        let cascade_epoch = ledger.epoch();
        ledger.record("gemini-4-neon-high", false, tokens(100, 0, 0), 10);
        ledger.reset(20);
        ledger.record("gemini-4-neon-high", false, tokens(50, 0, 0), 30);
        // The cascade that started before the reset unwinds with its authoritative total.
        ledger.reconcile(
            "gemini-4-neon-high",
            cascade_epoch,
            tokens(100, 0, 0),
            tokens(110, 0, 0),
            40,
        );
        let row = ledger.rows().first().expect("row");
        assert_eq!(row.tokens.input, 50, "post-reset usage untouched");
        assert_ne!(ledger.epoch(), cascade_epoch);
    }

    #[test]
    fn reconcile_spanning_a_price_change_keeps_the_models_total_exact() {
        // A cascade recorded 100 input tokens before Carbon's price change and 10 after it,
        // but upstream billed 90 in total.
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(100, 0, 5),
            DEC_1_2026 - 1,
        );
        ledger.record(
            "gemini-4-carbon-high",
            false,
            tokens(10, 0, 5),
            DEC_1_2026 + 1,
        );
        ledger.reconcile(
            "gemini-4-carbon-high",
            ledger.epoch(),
            tokens(110, 0, 10),
            tokens(90, 0, 12),
            DEC_1_2026 + 2,
        );
        let total = ledger
            .rows()
            .iter()
            .fold(UsageTokens::default(), |acc, row| {
                acc.saturating_add(row.tokens)
            });
        assert_eq!((total.input, total.output), (90, 12));
        let newer = ledger
            .rows()
            .iter()
            .find(|row| row.period_start_unix_ms == DEC_1_2026)
            .expect("post-change row");
        assert_eq!(newer.tokens.input, 0, "drained first");
        assert_eq!(newer.tokens.output, 7, "growth lands on the newest period");
    }

    #[test]
    fn ledger_rows_are_bounded_and_ids_capped() {
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        for i in 0..(MAX_LEDGER_ROWS + 5) {
            let id = format!("model-{i}");
            ledger.record(
                &id,
                false,
                tokens(1, 0, 1),
                u64::try_from(i).unwrap_or(0) + 10,
            );
        }
        assert_eq!(ledger.rows().len(), MAX_LEDGER_ROWS);
        assert!(ledger.rows().iter().all(|row| row.model_id != "model-0"));
        let long = "é".repeat(400);
        ledger.record(&long, false, tokens(1, 0, 1), 99_999);
        assert!(
            ledger
                .rows()
                .iter()
                .all(|row| row.model_id.len() <= MAX_PRICING_KEY_BYTES)
        );
    }

    #[test]
    fn pricing_report_lists_defaults_custom_and_seen_models() {
        let mut ledger = UsageLedger::from_persisted(1, Vec::new(), 1);
        ledger.record("brand-new-model-low", false, tokens(1, 0, 1), 5);
        let mut prices = PriceBook::load(None);
        let _ = prices.set(
            "zeta-model",
            Some(ModelPrice {
                input: 1.0,
                cached_input: 0.5,
                cache_write: 1.0,
                output: 2.0,
            }),
        );
        let report = prices.report(
            &catalog(&[
                "gemini-4-carbon-high",
                "gemini-pro-orchestrator",
                "gemini-4-carbon-low",
            ]),
            &ledger,
            DEC_1_2026 - 1,
        );
        assert_eq!(report.currency, "USD");
        assert_eq!(report.defaults_as_of, DEFAULT_PRICING_AS_OF);
        let keys: Vec<&str> = report.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys.len(), DEFAULT_PRICING.len() + 2);
        assert_eq!(
            &keys[DEFAULT_PRICING.len()..],
            ["brand-new-model", "zeta-model"]
        );
        let carbon = report
            .entries
            .iter()
            .find(|e| e.key == "gemini-4-carbon")
            .expect("carbon");
        assert_eq!(
            carbon.model_ids,
            [
                "gemini-4-carbon-high",
                "gemini-4-carbon-low",
                "gemini-pro-orchestrator"
            ]
        );
        assert_eq!(carbon.default_price.map(|p| p.input), Some(2.0));
        assert_eq!(carbon.upcoming_defaults.len(), 1);
        let unknown = report
            .entries
            .iter()
            .find(|e| e.key == "brand-new-model")
            .expect("unknown");
        assert_eq!(unknown.model_ids, ["brand-new-model-low"]);
        assert!(unknown.default_price.is_none() && unknown.custom_price.is_none());
        let zeta = report
            .entries
            .iter()
            .find(|e| e.key == "zeta-model")
            .expect("zeta");
        assert_eq!(zeta.custom_price.map(|p| p.output), Some(2.0));
    }

    #[test]
    fn price_book_round_trips_and_skips_invalid_entries() {
        let dir = std::env::temp_dir().join(format!("liftgate-pricing-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("mkdir: {err}"));
        let path = dir.join("model-pricing.json");
        let mut prices = PriceBook::load(Some(path.clone()));
        let _ = prices.set(
            "gpt-oss-120b",
            Some(ModelPrice {
                input: 0.1,
                cached_input: 0.1,
                cache_write: 0.1,
                output: 0.4,
            }),
        );
        let (target, _, bytes) = prices
            .prepare_persist()
            .unwrap_or_else(|| panic!("persist"));
        fs::write(&target, bytes).unwrap_or_else(|err| panic!("write: {err}"));
        assert_eq!(
            PriceBook::load(Some(path.clone()))
                .price_for("gpt-oss-120b", 0)
                .map(|p| p.output),
            Some(0.4)
        );
        fs::write(
            &path,
            br#"{"version":1,"custom_prices":{"bad":{"input":-1,"cached_input":0,"cache_write":0,"output":0},"ok":{"input":1,"cached_input":1,"cache_write":1,"output":1}}}"#,
        )
        .unwrap_or_else(|err| panic!("write: {err}"));
        let reloaded = PriceBook::load(Some(path));
        assert!(reloaded.price_for("bad", 0).is_none());
        assert!(reloaded.price_for("ok", 0).is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
