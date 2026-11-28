//! Per-model token usage ledger and the price book that turns it into USD.
//!
//! Every completed request lands twice: in an all-time row keyed by model and default price
//! period, and in a quarter-hour history bucket of its model. The rows keep all-time totals exact
//! and cheap to price. The buckets let the dashboard total any day or range in the viewer's own
//! time zone: every UTC offset in use is a whole number of quarter hours, so each local midnight
//! starts a bucket. History is capped at [`MAX_HISTORY_BUCKETS`]; the oldest buckets go first and
//! only the all-time rows remember them.
//!
//! Costs are derived on read from the price book. A default price applies to the usage of its own
//! period, so an announced price change never re-prices earlier usage, while custom prices
//! (persisted in `model-pricing.json`) replace the defaults for everything recorded.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use liftgate_types::acp::AvailableModel;
use liftgate_types::pricing::{
    DEFAULT_PRICING, DEFAULT_PRICING_AS_OF, MAX_PRICING_KEY_BYTES, ModelPrice, ModelPricingEntry,
    ModelUsage, PricingFamily, PricingResponse, ProviderUsage, UsageInterval, UsageResponse,
    UsageSeriesResponse, UsageSummary, UsageTokens, default_family, family_for_model, pricing_key,
};
use serde::{Deserialize, Serialize};

/// Width of a history bucket: a quarter hour, so local midnight in every time zone in use starts
/// a bucket.
pub const HISTORY_BUCKET_MS: u64 = 15 * 60 * 1000;
/// Most history buckets kept across all models: 520 days of one model used around the clock.
pub const MAX_HISTORY_BUCKETS: usize = 50_000;
/// Buckets dropped at once beyond the cap, so pruning stays rare.
const HISTORY_PRUNE_BATCH: usize = MAX_HISTORY_BUCKETS / 32;
/// Most bounds one series request may carry: a little over a year of days.
pub const MAX_SERIES_BOUNDS: usize = 400;
/// Most (model, price period) rows kept; the least recently used row is dropped beyond it.
pub const MAX_LEDGER_ROWS: usize = 512;
/// Most custom prices kept.
pub const MAX_CUSTOM_PRICES: usize = 256;
/// Largest usage ledger file read back; a bigger one is ignored as corrupt.
const MAX_LEDGER_FILE_BYTES: u64 = 64 * 1024 * 1024;
const LEDGER_SCHEMA_VERSION: u32 = 1;
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

fn default_agy_provider() -> String {
    "agy".to_owned()
}

fn encode_history_key(provider_id: &str, model_id: &str) -> String {
    let p = provider_id.trim();
    if p.is_empty() || p == "agy" {
        model_id.to_owned()
    } else {
        format!("{p}\t{model_id}")
    }
}

fn split_history_key(raw_key: &str) -> (&str, &str) {
    match raw_key.split_once('\t') {
        Some((prov, model)) if !prov.is_empty() => (prov, model),
        _ => ("agy", raw_key),
    }
}

/// All-time usage of one model within one default price period.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedgerRow {
    #[serde(default = "default_agy_provider")]
    pub provider_id: String,
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

/// One quarter hour of one model's usage, persisted as `[slot, calls, auxiliary_calls, input,
/// cached_input, cache_write, output, thought]` to keep the ledger file compact. Missing trailing
/// fields read as zero and extra ones are ignored, so the layout can grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "Vec<u64>", into = "[u64; 8]")]
struct Bucket {
    /// `start_unix_ms / HISTORY_BUCKET_MS`.
    slot: u64,
    calls: u64,
    auxiliary_calls: u64,
    tokens: UsageTokens,
}

impl From<Vec<u64>> for Bucket {
    fn from(raw: Vec<u64>) -> Self {
        let mut fields = raw.into_iter();
        let mut next = || fields.next().unwrap_or(0);
        // Struct expressions evaluate their fields in source order.
        Self {
            slot: next(),
            calls: next(),
            auxiliary_calls: next(),
            tokens: UsageTokens {
                input: next(),
                cached_input: next(),
                cache_write: next(),
                output: next(),
                thought: next(),
            },
        }
    }
}

impl From<Bucket> for [u64; 8] {
    fn from(b: Bucket) -> Self {
        [
            b.slot,
            b.calls,
            b.auxiliary_calls,
            b.tokens.input,
            b.tokens.cached_input,
            b.tokens.cache_write,
            b.tokens.output,
            b.tokens.thought,
        ]
    }
}

impl Bucket {
    const fn start_unix_ms(&self) -> u64 {
        self.slot.saturating_mul(HISTORY_BUCKET_MS)
    }

    const fn absorb(&mut self, other: &Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.auxiliary_calls = self.auxiliary_calls.saturating_add(other.auxiliary_calls);
        self.tokens = self.tokens.saturating_add(other.tokens);
    }
}

/// On-disk form of [`UsageLedger`] (`usage-ledger.json`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedLedger {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    since_unix_ms: u64,
    #[serde(default)]
    pruned_before_unix_ms: u64,
    #[serde(default)]
    totals: Vec<LedgerRow>,
    #[serde(default)]
    history: BTreeMap<String, VecDeque<Bucket>>,
}

/// A ledger snapshot taken under the hub lock and serialized by the writer outside it.
#[derive(Debug)]
pub struct PreparedLedger {
    pub path: PathBuf,
    /// Orders concurrent writers: only a newer snapshot may replace the file.
    pub seq: u64,
    payload: PersistedLedger,
}

impl PreparedLedger {
    /// Compact JSON of the snapshot.
    ///
    /// # Errors
    ///
    /// Returns the serializer error, which only a broken writer can produce.
    pub fn to_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&self.payload)
    }
}

/// Cumulative token usage per model since `since_unix_ms`.
#[derive(Clone, Debug)]
pub struct UsageLedger {
    path: Option<PathBuf>,
    /// All-time usage per (model, default price period).
    rows: Vec<LedgerRow>,
    /// Recent usage per model, ascending by slot.
    history: BTreeMap<String, VecDeque<Bucket>>,
    bucket_count: usize,
    since_unix_ms: u64,
    /// History before this instant was dropped to stay within [`MAX_HISTORY_BUCKETS`].
    pruned_before_unix_ms: u64,
    revision: u64,
    /// Bumped by [`Self::reset`]; corrections from cascades that began before a reset are dropped.
    epoch: u64,
    persist_seq: u64,
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

fn read_persisted_ledger(path: &Path) -> Option<PersistedLedger> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = ?err, "failed to open usage ledger; starting empty");
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(err) = file
        .take(MAX_LEDGER_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
    {
        tracing::warn!(path = %path.display(), error = ?err, "failed to read usage ledger; starting empty");
        return None;
    }
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_LEDGER_FILE_BYTES) {
        tracing::warn!(path = %path.display(), "usage ledger exceeds its size cap; starting empty");
        return None;
    }
    match serde_json::from_slice(&bytes) {
        Ok(ledger) => Some(ledger),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = ?err, "corrupt usage ledger; starting empty");
            None
        }
    }
}

/// Buckets of one model starting within `[from, to)`.
fn buckets_within(
    buckets: &VecDeque<Bucket>,
    from: u64,
    to: u64,
) -> std::collections::vec_deque::Iter<'_, Bucket> {
    let lo = buckets.partition_point(|bucket| bucket.start_unix_ms() < from);
    let hi = buckets
        .partition_point(|bucket| bucket.start_unix_ms() < to)
        .max(lo);
    buckets.range(lo..hi)
}

/// Growth and shrinkage per token class taking `accounted` to `authoritative`.
fn corrections(accounted: UsageTokens, authoritative: UsageTokens) -> [(u64, u64); 5] {
    let mut deltas = [(0, 0); 5];
    for (delta, field) in deltas.iter_mut().zip(TOKEN_FIELDS) {
        let (mut acc, mut auth) = (accounted, authoritative);
        let (acc, auth) = (*field(&mut acc), *field(&mut auth));
        *delta = (auth.saturating_sub(acc), acc.saturating_sub(auth));
    }
    deltas
}

/// Applies `deltas` to `slots`, newest first: growth lands on the newest slot and shrinkage
/// drains the newest first, carrying into older ones, so no part of a correction is lost to
/// saturation.
fn apply_correction<'a>(
    slots: impl Iterator<Item = &'a mut UsageTokens>,
    mut deltas: [(u64, u64); 5],
) {
    for (index, tokens) in slots.enumerate() {
        for ((growth, shrink), field) in deltas.iter_mut().zip(TOKEN_FIELDS) {
            let value = field(tokens);
            if index == 0 {
                *value = value.saturating_add(*growth);
            }
            let take = (*shrink).min(*value);
            *value -= take;
            *shrink -= take;
        }
        if deltas.iter().all(|(_, shrink)| *shrink == 0) {
            break;
        }
    }
}

/// Adds `(calls, auxiliary_calls)` and `tokens` priced at `price` (unpriced when `None`) to a
/// model's totals.
fn add_usage(
    entry: &mut ModelUsage,
    cache_savings_usd: &mut f64,
    price: Option<ModelPrice>,
    calls: (u64, u64),
    tokens: &UsageTokens,
) {
    let (calls, auxiliary_calls) = calls;
    entry.calls = entry.calls.saturating_add(calls);
    entry.auxiliary_calls = entry.auxiliary_calls.saturating_add(auxiliary_calls);
    entry.tokens = entry.tokens.saturating_add(*tokens);
    entry.cost_usd = match (entry.cost_usd, price) {
        (Some(total), Some(price)) => Some(total + price.cost_usd(tokens)),
        _ => None,
    };
    if let Some(price) = price {
        *cache_savings_usd += price.cache_savings_usd(tokens);
    }
}

fn add_provider_usage(
    entry: &mut ProviderUsage,
    price: Option<ModelPrice>,
    calls: (u64, u64),
    tokens: &UsageTokens,
) {
    let (calls, auxiliary_calls) = calls;
    entry.calls = entry.calls.saturating_add(calls);
    entry.auxiliary_calls = entry.auxiliary_calls.saturating_add(auxiliary_calls);
    entry.tokens = entry.tokens.saturating_add(*tokens);
    match price {
        Some(price) => {
            entry.cost_usd += price.cost_usd(tokens);
            entry.cache_savings_usd += price.cache_savings_usd(tokens);
        }
        None => {
            entry.unpriced_calls = entry.unpriced_calls.saturating_add(calls);
        }
    }
}

/// The per-model entry of `model_id`, widened to cover `[first_used, last_used]`.
fn model_entry<'a>(
    by_model: &'a mut BTreeMap<String, ModelUsage>,
    model_id: &str,
    first_used: u64,
    last_used: u64,
) -> &'a mut ModelUsage {
    let entry = by_model
        .entry(model_id.to_owned())
        .or_insert_with(|| ModelUsage {
            model_id: model_id.to_owned(),
            pricing_key: pricing_key(model_id).to_owned(),
            cost_usd: Some(0.0),
            first_used_at_unix_ms: first_used,
            last_used_at_unix_ms: last_used,
            ..ModelUsage::default()
        });
    entry.first_used_at_unix_ms = entry.first_used_at_unix_ms.min(first_used);
    entry.last_used_at_unix_ms = entry.last_used_at_unix_ms.max(last_used);
    entry
}

impl UsageLedger {
    /// Restores the ledger persisted at `path` (kept in memory only without one); a ledger
    /// without a start time starts at `now_ms`.
    #[must_use]
    pub fn load(path: Option<PathBuf>, now_ms: u64) -> Self {
        let persisted = path
            .as_deref()
            .and_then(read_persisted_ledger)
            .unwrap_or_default();
        let mut rows: Vec<LedgerRow> = persisted
            .totals
            .into_iter()
            .filter_map(|mut row| {
                let id = bounded_id(&row.model_id).to_owned();
                if id.is_empty() {
                    return None;
                }
                row.model_id = id;
                Some(row)
            })
            .collect();
        if rows.len() > MAX_LEDGER_ROWS {
            rows.sort_by_key(|row| std::cmp::Reverse(row.last_used_at_unix_ms));
            rows.truncate(MAX_LEDGER_ROWS);
        }
        let mut merged: BTreeMap<String, Vec<Bucket>> = BTreeMap::new();
        for (model_id, buckets) in persisted.history {
            let id = bounded_id(&model_id);
            if !id.is_empty() {
                merged.entry(id.to_owned()).or_default().extend(buckets);
            }
        }
        let mut history = BTreeMap::new();
        for (model_id, mut buckets) in merged {
            buckets.sort_by_key(|bucket| bucket.slot);
            buckets.dedup_by(|later, earlier| {
                let same = later.slot == earlier.slot;
                if same {
                    earlier.absorb(later);
                }
                same
            });
            if !buckets.is_empty() {
                history.insert(model_id, VecDeque::from(buckets));
            }
        }
        let mut ledger = Self {
            path,
            rows,
            bucket_count: history.values().map(VecDeque::len).sum(),
            history,
            since_unix_ms: if persisted.since_unix_ms > 0 {
                persisted.since_unix_ms
            } else {
                now_ms
            },
            pruned_before_unix_ms: persisted.pruned_before_unix_ms,
            revision: 1,
            epoch: 0,
            persist_seq: 0,
        };
        ledger.prune(MAX_HISTORY_BUCKETS);
        ledger
    }

    #[must_use]
    pub const fn since_unix_ms(&self) -> u64 {
        self.since_unix_ms
    }

    /// Where bucketed history starts; earlier usage survives only in the all-time totals.
    #[must_use]
    pub const fn history_from_unix_ms(&self) -> u64 {
        if self.pruned_before_unix_ms > self.since_unix_ms {
            self.pruned_before_unix_ms
        } else {
            self.since_unix_ms
        }
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

    /// Every model with recorded usage.
    #[must_use]
    pub fn model_ids(&self) -> BTreeSet<&str> {
        self.rows
            .iter()
            .map(|row| row.model_id.as_str())
            .chain(self.history.keys().map(|k| split_history_key(k).1))
            .collect()
    }

    fn row_mut(
        &mut self,
        provider_id: &str,
        model_id: &str,
        period: u64,
        now_ms: u64,
    ) -> Option<&mut LedgerRow> {
        let pos = match self.rows.iter().position(|row| {
            row.provider_id == provider_id
                && row.model_id == model_id
                && row.period_start_unix_ms == period
        }) {
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
                    provider_id: provider_id.to_owned(),
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

    fn add_bucket(&mut self, model_id: &str, incoming: Bucket) {
        if !self.history.contains_key(model_id) {
            self.history.insert(model_id.to_owned(), VecDeque::new());
        }
        let Some(buckets) = self.history.get_mut(model_id) else {
            return;
        };
        match buckets.binary_search_by_key(&incoming.slot, |bucket| bucket.slot) {
            Ok(pos) => {
                if let Some(bucket) = buckets.get_mut(pos) {
                    bucket.absorb(&incoming);
                }
            }
            Err(pos) => {
                buckets.insert(pos, incoming);
                self.bucket_count = self.bucket_count.saturating_add(1);
            }
        }
        if self.bucket_count > MAX_HISTORY_BUCKETS {
            self.prune(MAX_HISTORY_BUCKETS.saturating_sub(HISTORY_PRUNE_BATCH));
        }
    }

    /// Drops the oldest buckets (ties included) until at most `keep` remain.
    fn prune(&mut self, keep: usize) {
        let mut slots: Vec<u64> = self
            .history
            .values()
            .flatten()
            .map(|bucket| bucket.slot)
            .collect();
        self.bucket_count = slots.len();
        let Some(nth) = slots.len().saturating_sub(keep).checked_sub(1) else {
            return;
        };
        if nth >= slots.len() {
            return;
        }
        let (_, threshold, _) = slots.select_nth_unstable(nth);
        let threshold = *threshold;
        for buckets in self.history.values_mut() {
            while buckets
                .front()
                .is_some_and(|bucket| bucket.slot <= threshold)
            {
                buckets.pop_front();
            }
        }
        self.history.retain(|_, buckets| !buckets.is_empty());
        self.bucket_count = self.history.values().map(VecDeque::len).sum();
        self.pruned_before_unix_ms = self.pruned_before_unix_ms.max(
            threshold
                .saturating_add(1)
                .saturating_mul(HISTORY_BUCKET_MS),
        );
    }

    /// Adds one completed request.
    pub fn record(&mut self, model_id: &str, auxiliary: bool, tokens: UsageTokens, now_ms: u64) {
        self.record_with_provider("agy", model_id, auxiliary, tokens, now_ms);
    }

    /// Adds one completed request attributed to `provider_id`.
    pub fn record_with_provider(
        &mut self,
        provider_id: &str,
        model_id: &str,
        auxiliary: bool,
        tokens: UsageTokens,
        now_ms: u64,
    ) {
        let id = bounded_id(model_id);
        if id.is_empty() {
            return;
        }
        let prov = {
            let p = bounded_id(provider_id);
            if p.is_empty() { "agy" } else { p }
        };
        let period = period_start(id, now_ms);
        if let Some(row) = self.row_mut(prov, id, period, now_ms) {
            row.calls = row.calls.saturating_add(1);
            if auxiliary {
                row.auxiliary_calls = row.auxiliary_calls.saturating_add(1);
            }
            row.tokens = row.tokens.saturating_add(tokens);
            row.last_used_at_unix_ms = now_ms;
        }
        let hist_key = encode_history_key(prov, id);
        self.add_bucket(
            &hist_key,
            Bucket {
                slot: now_ms / HISTORY_BUCKET_MS,
                calls: 1,
                auxiliary_calls: u64::from(auxiliary),
                tokens,
            },
        );
        self.revision = self.revision.saturating_add(1);
    }

    /// Replaces step estimates already recorded for `model_id` with the authoritative usage
    /// reported once its MCP cascade finished.
    ///
    /// The all-time rows and the history are corrected alike: growth lands on the newest price
    /// period and bucket, and shrinkage drains the newest first, carrying into earlier ones, so a
    /// cascade that straddled a price change or a bucket boundary keeps the model's totals exact.
    /// A cascade that began before the latest reset (an older `epoch`) is ignored: its estimates
    /// were cleared with the rest.
    pub fn reconcile(
        &mut self,
        model_id: &str,
        epoch: u64,
        accounted: UsageTokens,
        authoritative: UsageTokens,
        now_ms: u64,
    ) {
        self.reconcile_with_provider("agy", model_id, epoch, accounted, authoritative, now_ms);
    }

    /// Like [`Self::reconcile`], scoped exclusively to `provider_id`'s rows and history buckets
    /// so Antigravity cascade drift corrections never mutate another provider sharing `model_id`.
    pub fn reconcile_with_provider(
        &mut self,
        provider_id: &str,
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
        if id.is_empty() {
            return;
        }
        let prov = {
            let p = bounded_id(provider_id);
            if p.is_empty() { "agy" } else { p }
        };
        let period = period_start(id, now_ms);
        let deltas = corrections(accounted, authoritative);
        let mut rows: Vec<&mut LedgerRow> = self
            .rows
            .iter_mut()
            .filter(|row| {
                row.provider_id == prov && row.model_id == id && row.period_start_unix_ms <= period
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.period_start_unix_ms));
        apply_correction(rows.into_iter().map(|row| &mut row.tokens), deltas);
        let hist_key = encode_history_key(prov, id);
        if let Some(buckets) = self.history.get_mut(&hist_key) {
            apply_correction(
                buckets.iter_mut().rev().map(|bucket| &mut bucket.tokens),
                deltas,
            );
        }
        self.revision = self.revision.saturating_add(1);
    }

    /// Drops all recorded usage and starts counting again from `now_ms`.
    pub fn reset(&mut self, now_ms: u64) {
        self.rows.clear();
        self.history.clear();
        self.bucket_count = 0;
        self.since_unix_ms = now_ms;
        self.pruned_before_unix_ms = 0;
        self.revision = self.revision.saturating_add(1);
        self.epoch = self.epoch.saturating_add(1);
    }

    /// Per-model usage, costliest first, plus totals: everything recorded when `range` is
    /// `None`, else the usage of buckets starting within `[from, to)`.
    #[must_use]
    pub fn report(&self, prices: &PriceBook, range: Option<(u64, u64)>) -> UsageResponse {
        let mut by_model: BTreeMap<String, ModelUsage> = BTreeMap::new();
        let mut by_provider: BTreeMap<String, ProviderUsage> = BTreeMap::new();
        let mut cache_savings_usd = 0.0;
        match range {
            None => {
                for row in &self.rows {
                    let price =
                        prices.price_for(pricing_key(&row.model_id), row.period_start_unix_ms);
                    let entry = model_entry(
                        &mut by_model,
                        &row.model_id,
                        row.first_used_at_unix_ms,
                        row.last_used_at_unix_ms,
                    );
                    add_usage(
                        entry,
                        &mut cache_savings_usd,
                        price,
                        (row.calls, row.auxiliary_calls),
                        &row.tokens,
                    );
                    let prov_key = if row.provider_id.is_empty() {
                        "agy"
                    } else {
                        row.provider_id.as_str()
                    };
                    add_provider_usage(
                        by_provider
                            .entry(prov_key.to_owned())
                            .or_insert_with(|| ProviderUsage {
                                provider_id: prov_key.to_owned(),
                                ..ProviderUsage::default()
                            }),
                        price,
                        (row.calls, row.auxiliary_calls),
                        &row.tokens,
                    );
                }
            }
            Some((from, to)) => {
                for (raw_key, buckets) in &self.history {
                    let (prov_id, model_id) = split_history_key(raw_key);
                    let rule = prices.rule_for(pricing_key(model_id));
                    for bucket in buckets_within(buckets, from, to) {
                        let start = bucket.start_unix_ms();
                        let p = rule.at(start);
                        let entry = model_entry(&mut by_model, model_id, start, start);
                        add_usage(
                            entry,
                            &mut cache_savings_usd,
                            p,
                            (bucket.calls, bucket.auxiliary_calls),
                            &bucket.tokens,
                        );
                        add_provider_usage(
                            by_provider.entry(prov_id.to_owned()).or_insert_with(|| {
                                ProviderUsage {
                                    provider_id: prov_id.to_owned(),
                                    ..ProviderUsage::default()
                                }
                            }),
                            p,
                            (bucket.calls, bucket.auxiliary_calls),
                            &bucket.tokens,
                        );
                    }
                }
            }
        }
        let mut models: Vec<ModelUsage> = by_model.into_values().collect();
        models.sort_by(|a, b| {
            b.cost_usd
                .unwrap_or(-1.0)
                .total_cmp(&a.cost_usd.unwrap_or(-1.0))
                .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
        });
        let mut providers: Vec<ProviderUsage> = by_provider.into_values().collect();
        providers.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
        });
        let mut summary = UsageSummary {
            revision: self.revision.saturating_add(prices.revision()),
            since_unix_ms: range.map_or(self.since_unix_ms, |(from, _)| from),
            until_unix_ms: range.map(|(_, to)| to),
            history_from_unix_ms: self.history_from_unix_ms(),
            cache_savings_usd,
            by_provider: providers.clone(),
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
        UsageResponse {
            summary,
            models,
            providers,
        }
    }

    /// Usage per interval between consecutive `bounds` (ascending instants, typically the
    /// caller's local midnights); each bucket counts toward the interval its start falls in.
    #[must_use]
    pub fn series(&self, prices: &PriceBook, bounds: &[u64]) -> UsageSeriesResponse {
        let mut intervals: Vec<UsageInterval> = bounds
            .windows(2)
            .filter_map(|pair| match *pair {
                [start, end] => Some(UsageInterval {
                    start_unix_ms: start,
                    end_unix_ms: end,
                    ..UsageInterval::default()
                }),
                _ => None,
            })
            .collect();
        if let (Some(&first), Some(&last)) = (bounds.first(), bounds.last()) {
            for (raw_key, buckets) in &self.history {
                let (prov_id, model_id) = split_history_key(raw_key);
                let rule = prices.rule_for(pricing_key(model_id));
                let mut cursor = 0;
                for bucket in buckets_within(buckets, first, last) {
                    let start = bucket.start_unix_ms();
                    while intervals
                        .get(cursor)
                        .is_some_and(|interval| interval.end_unix_ms <= start)
                    {
                        cursor += 1;
                    }
                    let Some(interval) = intervals.get_mut(cursor) else {
                        break;
                    };
                    if start < interval.start_unix_ms {
                        continue;
                    }
                    interval.calls = interval.calls.saturating_add(bucket.calls);
                    interval.auxiliary_calls = interval
                        .auxiliary_calls
                        .saturating_add(bucket.auxiliary_calls);
                    interval.tokens = interval.tokens.saturating_add(bucket.tokens);
                    let p = rule.at(start);
                    match p {
                        Some(price) => {
                            interval.cost_usd += price.cost_usd(&bucket.tokens);
                            interval.cache_savings_usd += price.cache_savings_usd(&bucket.tokens);
                        }
                        None => {
                            interval.unpriced_calls =
                                interval.unpriced_calls.saturating_add(bucket.calls);
                        }
                    }
                    let prov_entry = match interval
                        .by_provider
                        .iter_mut()
                        .find(|pu| pu.provider_id == prov_id)
                    {
                        Some(existing) => existing,
                        None => {
                            interval.by_provider.push(ProviderUsage {
                                provider_id: prov_id.to_owned(),
                                ..ProviderUsage::default()
                            });
                            let idx = interval.by_provider.len() - 1;
                            let Some(slot) = interval.by_provider.get_mut(idx) else {
                                continue;
                            };
                            slot
                        }
                    };
                    add_provider_usage(
                        prov_entry,
                        p,
                        (bucket.calls, bucket.auxiliary_calls),
                        &bucket.tokens,
                    );
                }
            }
        }
        UsageSeriesResponse {
            revision: self.revision.saturating_add(prices.revision()),
            bucket_ms: HISTORY_BUCKET_MS,
            history_from_unix_ms: self.history_from_unix_ms(),
            intervals,
        }
    }

    /// Snapshot for an atomic rewrite of the ledger file; `None` when kept in memory only.
    pub fn prepare_persist(&mut self) -> Option<PreparedLedger> {
        let path = self.path.clone()?;
        self.persist_seq = self.persist_seq.saturating_add(1);
        Some(PreparedLedger {
            path,
            seq: self.persist_seq,
            payload: PersistedLedger {
                version: LEDGER_SCHEMA_VERSION,
                since_unix_ms: self.since_unix_ms,
                pruned_before_unix_ms: self.pruned_before_unix_ms,
                totals: self.rows.clone(),
                history: self.history.clone(),
            },
        })
    }

    #[cfg(test)]
    fn rows(&self) -> &[LedgerRow] {
        &self.rows
    }

    #[cfg(test)]
    const fn bucket_count(&self) -> usize {
        self.bucket_count
    }
}

/// How one pricing key is priced over time.
#[derive(Clone, Copy, Debug)]
enum PriceRule {
    Custom(ModelPrice),
    Default(&'static PricingFamily),
    Unpriced,
}

impl PriceRule {
    fn at(self, unix_ms: u64) -> Option<ModelPrice> {
        match self {
            Self::Custom(price) => Some(price),
            Self::Default(family) => family.price_at(unix_ms),
            Self::Unpriced => None,
        }
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

    fn rule_for(&self, key: &str) -> PriceRule {
        match self.custom.get(key) {
            Some(price) => PriceRule::Custom(*price),
            None => default_family(key).map_or(PriceRule::Unpriced, PriceRule::Default),
        }
    }

    /// Price for `key` at `unix_ms`: the custom price, else the default then in force.
    #[must_use]
    pub fn price_for(&self, key: &str, unix_ms: u64) -> Option<ModelPrice> {
        self.rule_for(key).at(unix_ms)
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
            .chain(ledger.model_ids());
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
        let mut ledger = UsageLedger::load(None, 1_000);
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
        let report = ledger.report(&prices, None);
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
        let repriced = ledger.report(&prices, None);
        assert!((repriced.summary.cost_usd - 6.0).abs() < 1e-9);
        assert_eq!(repriced.summary.unpriced_calls, 0);
        assert!(repriced.summary.revision > report.summary.revision);
        assert_eq!(prices.set("mystery-model", None), PriceUpdate::Changed);
        assert_eq!(prices.set("mystery-model", None), PriceUpdate::Unchanged);
        assert_eq!(ledger.report(&prices, None).summary.unpriced_calls, 1);

        ledger.reset(5_000);
        assert!(ledger.rows().is_empty());
        assert_eq!(ledger.since_unix_ms(), 5_000);
    }

    #[test]
    fn cache_savings_follow_the_price_of_each_period() {
        // 1M cached Carbon tokens on each side of the Dec 1 change: $1.90 then $3.80 saved.
        let mut ledger = UsageLedger::load(None, 1);
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
        let saved = ledger.report(&prices, None).summary.cache_savings_usd;
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
        let saved = ledger.report(&prices, None).summary.cache_savings_usd;
        assert!((saved - 1.0).abs() < 1e-9, "{saved}");
    }

    #[test]
    fn reconcile_corrects_the_models_latest_period() {
        let mut ledger = UsageLedger::load(None, 1);
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
        let mut ledger = UsageLedger::load(None, 1);
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
        let mut ledger = UsageLedger::load(None, 1);
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
        let mut ledger = UsageLedger::load(None, 1);
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
        let mut ledger = UsageLedger::load(None, 1);
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

    const HOUR: u64 = 3_600_000;
    const DAY: u64 = 24 * HOUR;
    /// Local midnight starting day `d` (days since the epoch) at UTC offset `offset_ms`.
    const fn local_midnight(d: u64, offset_ms: u64) -> u64 {
        d * DAY - offset_ms
    }

    #[test]
    fn ranges_and_series_follow_the_callers_local_midnights() {
        let shanghai = 8 * HOUR;
        let (d10, d11, d12) = (
            local_midnight(10, shanghai),
            local_midnight(11, shanghai),
            local_midnight(12, shanghai),
        );
        let mut ledger = UsageLedger::load(None, 1);
        // Neon is $3 in / $0.15 cached / $15 out, with no scheduled change.
        ledger.record(
            "gemini-4-neon-high",
            false,
            tokens(1_000_000, 0, 0),
            d10 + HOUR,
        );
        ledger.record(
            "gemini-4-neon-high",
            true,
            tokens(0, 0, 1_000_000),
            d11 - 60_000,
        );
        ledger.record("gemini-4-neon-low", false, tokens(0, 1_000_000, 0), d11);
        ledger.record("mystery-model", false, tokens(5, 0, 5), d11 + 2 * HOUR);
        let prices = PriceBook::load(None);

        let series = ledger.series(&prices, &[d10, d11, d12]);
        assert_eq!(series.bucket_ms, HISTORY_BUCKET_MS);
        assert_eq!(series.history_from_unix_ms, 1);
        let [first, second] = series.intervals.as_slice() else {
            panic!("two days: {:?}", series.intervals);
        };
        assert_eq!((first.start_unix_ms, first.end_unix_ms), (d10, d11));
        assert_eq!(
            (first.calls, first.auxiliary_calls),
            (2, 1),
            "23:59 stays on day 10"
        );
        assert!((first.cost_usd - 18.0).abs() < 1e-9, "{}", first.cost_usd);
        assert_eq!((second.calls, second.unpriced_calls), (2, 1));
        assert!((second.cost_usd - 0.15).abs() < 1e-9, "{}", second.cost_usd);
        assert!((second.cache_savings_usd - 2.85).abs() < 1e-9);

        let day11 = ledger.report(&prices, Some((d11, d12)));
        assert_eq!(day11.summary.since_unix_ms, d11);
        assert_eq!(day11.summary.until_unix_ms, Some(d12));
        assert_eq!(day11.summary.calls, 2);
        assert_eq!(day11.summary.unpriced_calls, 1);
        let ids: Vec<&str> = day11.models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(ids, ["gemini-4-neon-low", "mystery-model"]);
        let all = ledger.report(&prices, None);
        assert_eq!(all.summary.until_unix_ms, None);
        assert_eq!(all.summary.calls, 4);
        let ranged = ledger.report(&prices, Some((0, u64::MAX)));
        assert!((ranged.summary.cost_usd - all.summary.cost_usd).abs() < 1e-9);

        // Nepal is UTC+5:45: its midnight still starts a bucket.
        let nepal = 5 * HOUR + 45 * 60_000;
        let midnight = local_midnight(20, nepal);
        let mut ledger = UsageLedger::load(None, 1);
        ledger.record(
            "gemini-4-neon-high",
            false,
            tokens(1, 0, 0),
            midnight - 60_000,
        );
        ledger.record(
            "gemini-4-neon-high",
            false,
            tokens(2, 0, 0),
            midnight + 60_000,
        );
        let days = ledger.series(&prices, &[midnight - DAY, midnight, midnight + DAY]);
        let inputs: Vec<u64> = days.intervals.iter().map(|d| d.tokens.input).collect();
        assert_eq!(inputs, [1, 2]);
    }

    #[test]
    fn history_is_capped_and_drops_its_oldest_buckets_first() {
        let mut ledger = UsageLedger::load(None, 1);
        let start = 1_000 * HISTORY_BUCKET_MS;
        let mut at = start;
        for _ in 0..=MAX_HISTORY_BUCKETS {
            ledger.record("gemini-4-neon-high", false, tokens(1, 0, 0), at);
            at += HISTORY_BUCKET_MS;
        }
        let kept = MAX_HISTORY_BUCKETS - HISTORY_PRUNE_BATCH;
        assert_eq!(ledger.bucket_count(), kept);
        let dropped = u64::try_from(MAX_HISTORY_BUCKETS + 1 - kept).unwrap_or(0);
        assert_eq!(
            ledger.history_from_unix_ms(),
            start + dropped * HISTORY_BUCKET_MS
        );
        let prices = PriceBook::load(None);
        let all = ledger.report(&prices, None).summary;
        assert_eq!(
            all.calls,
            u64::try_from(MAX_HISTORY_BUCKETS + 1).unwrap_or(0),
            "all-time totals keep pruned usage"
        );
        assert_eq!(all.history_from_unix_ms, ledger.history_from_unix_ms());
        let recent = ledger.report(&prices, Some((0, u64::MAX))).summary;
        assert_eq!(recent.calls, u64::try_from(kept).unwrap_or(0));
    }

    #[test]
    fn reconcile_corrects_totals_and_history_alike() {
        let mut ledger = UsageLedger::load(None, 1);
        let t = 40 * HISTORY_BUCKET_MS;
        ledger.record("gemini-4-neon-high", false, tokens(100, 0, 0), t);
        ledger.record(
            "gemini-4-neon-high",
            false,
            tokens(100, 0, 0),
            t + HISTORY_BUCKET_MS,
        );
        ledger.reconcile(
            "gemini-4-neon-high",
            ledger.epoch(),
            tokens(200, 0, 0),
            tokens(50, 0, 10),
            t + HISTORY_BUCKET_MS,
        );
        let row = ledger.rows().first().expect("row");
        assert_eq!((row.tokens.input, row.tokens.output), (50, 10));
        let prices = PriceBook::load(None);
        let buckets = ledger.series(
            &prices,
            &[t, t + HISTORY_BUCKET_MS, t + 2 * HISTORY_BUCKET_MS],
        );
        let per_bucket: Vec<(u64, u64)> = buckets
            .intervals
            .iter()
            .map(|b| (b.tokens.input, b.tokens.output))
            .collect();
        assert_eq!(per_bucket, [(50, 0), (0, 10)], "newest drained first");
    }

    #[test]
    fn ledger_file_round_trips_and_tolerates_layout_changes() {
        let dir = std::env::temp_dir().join(format!("liftgate-usage-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("mkdir: {err}"));
        let path = dir.join("usage-ledger.json");
        let prices = PriceBook::load(None);
        let mut ledger = UsageLedger::load(Some(path.clone()), 7);
        ledger.record("gemini-4-neon-high", true, tokens(10, 20, 30), 5 * DAY);
        ledger.record("gpt-oss-120b", false, tokens(1, 2, 3), 6 * DAY);
        let prepared = ledger.prepare_persist().unwrap_or_else(|| panic!("path"));
        let bytes = prepared
            .to_bytes()
            .unwrap_or_else(|err| panic!("serialize: {err}"));
        fs::write(&prepared.path, bytes).unwrap_or_else(|err| panic!("write: {err}"));
        let restored = UsageLedger::load(Some(path.clone()), 99);
        assert_eq!(restored.since_unix_ms(), 7);
        // Revisions restart with the process; everything else is restored.
        let (mut before, mut after) =
            (ledger.report(&prices, None), restored.report(&prices, None));
        before.summary.revision = 0;
        after.summary.revision = 0;
        assert_eq!(after, before);
        let bounds = [4 * DAY, 5 * DAY, 6 * DAY, 7 * DAY];
        assert_eq!(
            restored.series(&prices, &bounds).intervals,
            ledger.series(&prices, &bounds).intervals
        );

        // Short buckets read missing fields as zero, extra fields are ignored, and duplicate
        // slots merge.
        fs::write(
            &path,
            br#"{"version":1,"since_unix_ms":3,"history":{"gemini-4-neon-high":[[10,1,0,5],[10,2,1,1,2,3,4,5,99,98],[9,1]]}}"#,
        )
        .unwrap_or_else(|err| panic!("write: {err}"));
        let tolerant = UsageLedger::load(Some(path.clone()), 99);
        let days = tolerant.series(&prices, &[0, 10 * HISTORY_BUCKET_MS, DAY]);
        let calls: Vec<u64> = days.intervals.iter().map(|d| d.calls).collect();
        assert_eq!(calls, [1, 3]);
        assert_eq!(
            days.intervals
                .last()
                .map(|d| (d.auxiliary_calls, d.tokens.input, d.tokens.thought)),
            Some((1, 6, 5))
        );

        fs::write(&path, b"{not json").unwrap_or_else(|err| panic!("write: {err}"));
        let fresh = UsageLedger::load(Some(path), 42);
        assert_eq!(fresh.since_unix_ms(), 42);
        assert_eq!(fresh.report(&prices, None).summary.calls, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_scopes_adjustments_to_target_provider_rows_and_history() {
        let prices = PriceBook::default();
        let mut ledger = UsageLedger::load(None, 1_000);
        let epoch = ledger.epoch();
        let now = 5 * HISTORY_BUCKET_MS + 100;

        // Insert native provider row FIRST for the same model and price period.
        let native_tok = UsageTokens {
            input: 500,
            cached_input: 100,
            cache_write: 0,
            output: 50,
            thought: 0,
        };
        let agy_est = UsageTokens {
            input: 300,
            cached_input: 0,
            cache_write: 0,
            output: 20,
            thought: 0,
        };
        ledger.record_with_provider("gemini", "gemini-3-pro", false, native_tok, now);
        ledger.record_with_provider("agy", "gemini-3-pro", false, agy_est, now + 10);

        // Reconcile Antigravity upward (+200 input) and downward (-10 output).
        let agy_auth = UsageTokens {
            input: 500,
            cached_input: 50,
            cache_write: 0,
            output: 10,
            thought: 0,
        };
        ledger.reconcile("gemini-3-pro", epoch, agy_est, agy_auth, now + 20);

        let all_time = ledger.report(&prices, None);
        let gem_prov = all_time
            .providers
            .iter()
            .find(|p| p.provider_id == "gemini")
            .expect("gemini provider summary");
        let agy_prov = all_time
            .providers
            .iter()
            .find(|p| p.provider_id == "agy")
            .expect("agy provider summary");
        assert_eq!(
            gem_prov.tokens, native_tok,
            "native provider row must stay untouched"
        );
        assert_eq!(
            agy_prov.tokens, agy_auth,
            "agy provider row must absorb full delta"
        );

        let ranged = ledger.report(&prices, Some((0, 10 * HISTORY_BUCKET_MS)));
        let gem_ranged = ranged
            .providers
            .iter()
            .find(|p| p.provider_id == "gemini")
            .expect("gemini ranged summary");
        let agy_ranged = ranged
            .providers
            .iter()
            .find(|p| p.provider_id == "agy")
            .expect("agy ranged summary");
        assert_eq!(gem_ranged.tokens, native_tok);
        assert_eq!(agy_ranged.tokens, agy_auth);
    }
}
