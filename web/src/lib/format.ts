import { msg } from "@/lib/i18n";
import type { TaskSnapshot } from "@/types/telemetry";

export function formatDurationMs(ms: number | null | undefined): string {
  if (typeof ms !== "number" || !Number.isFinite(ms)) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const secs = ms / 1000;
  if (secs < 60) return `${secs.toFixed(1)} s`;
  const mins = Math.floor(secs / 60);
  const rem = Math.round(secs % 60);
  return `${mins}m ${rem}s`;
}

export function formatUptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${secs % 60}s`;
  return `${secs}s`;
}

const pad2 = (n: number): string => String(n).padStart(2, "0");

/** `HH:MM:SS` in local time. */
export function formatClock(ms: number): string {
  if (ms <= 0) return "—";
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/** `MM-DD HH:MM:SS` in local time. */
export function formatDateTime(ms: number): string {
  if (ms <= 0) return "—";
  const d = new Date(ms);
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${formatClock(ms)}`;
}

/** Clock for today, date + clock for anything older. */
export function formatSmartTime(ms: number): string {
  if (ms <= 0) return "—";
  const d = new Date(ms);
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  return sameDay ? formatClock(ms) : formatDateTime(ms);
}

export function formatNumber(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatCompact(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 100_000_000 ? 0 : 1)}M`;
  if (n >= 10_000) return `${(n / 1_000).toFixed(n >= 100_000 ? 0 : 1)}K`;
  return n.toLocaleString("en-US");
}

export function formatBpsPct(bps: number | null | undefined, digits = 1): string {
  if (typeof bps !== "number" || !Number.isFinite(bps)) return "—";
  return `${(bps / 100).toFixed(digits)}%`;
}

export function formatPct(ratio: number | null | undefined, digits = 0): string {
  if (typeof ratio !== "number" || !Number.isFinite(ratio)) return "—";
  return `${(ratio * 100).toFixed(digits)}%`;
}

export function formatBytes(bytes: number | null | undefined): string {
  if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${Math.round(bytes)} B`;
}

export function shortId(id: string | null | undefined, keep = 8): string {
  if (!id) return "—";
  return id.length > keep + 1 ? `${id.slice(0, keep)}…` : id;
}

export function protocolPath(protocol: TaskSnapshot["protocol"]): string {
  return protocol === "openai_responses" ? "/v1/responses" : "/v1/messages";
}

export function taskPromptTokens(task: TaskSnapshot): number {
  const u = task.usage;
  if (!u) return 0;
  return u.inputTokens + u.cachedReadTokens + u.cachedWriteTokens;
}

export function taskHitBps(task: TaskSnapshot): number | null {
  if (typeof task.cache_hit_rate_bps === "number") return task.cache_hit_rate_bps;
  const prompt = taskPromptTokens(task);
  if (prompt <= 0 || !task.usage) return null;
  return Math.round((Math.min(task.usage.cachedReadTokens, prompt) * 10000) / prompt);
}

export function percentile(sorted: number[], p: number): number | null {
  if (sorted.length === 0) return null;
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[idx] ?? null;
}

export function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** Size change as a signed percentage: `−73%`, `+12%`, or "unchanged" when it rounds to nothing. */
export function formatSizeChange(before: number, after: number): string {
  if (before <= 0) return "—";
  const delta = (after - before) / before;
  if (Math.abs(delta) < 0.005) return msg().common.sizeUnchanged;
  return `${delta < 0 ? "−" : "+"}${Math.abs(delta * 100).toFixed(0)}%`;
}

/**
 * US dollars with precision that follows the amount: cents for dollars, more digits for the
 * fractions of a cent a single cached request costs.
 */
export function formatUsd(usd: number | null | undefined): string {
  if (typeof usd !== "number" || !Number.isFinite(usd)) return "—";
  if (usd === 0) return "$0.00";
  const abs = Math.abs(usd);
  const sign = usd < 0 ? "−" : "";
  if (abs < 0.0001) return `${sign}<$0.0001`;
  if (abs < 0.01) return `${sign}$${abs.toFixed(4)}`;
  if (abs < 1) return `${sign}$${abs.toFixed(3)}`;
  return `${sign}$${abs.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

/** A per-1M-token price: at least two decimals, up to four, trailing zeros trimmed. */
export function formatPrice(usd: number | null | undefined): string {
  if (typeof usd !== "number" || !Number.isFinite(usd)) return "—";
  const fixed = usd.toFixed(4).replace(/0{1,2}$/, "");
  return `$${fixed}`;
}
