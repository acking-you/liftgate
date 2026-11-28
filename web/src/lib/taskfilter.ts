import { percentile, taskHitBps, taskPromptTokens } from "@/lib/format";
import type { ToolOutcomeCounts } from "@/lib/toolcalls";
import type { TaskProtocol, TaskSnapshot } from "@/types/telemetry";

/** Filter key for turns whose harness is not (yet) attributed. */
export const UNKNOWN_HARNESS = "unknown";

export interface TaskFilter {
  protocol: "all" | TaskProtocol;
  /** Upstream provider id (`agy`, `openai`, `anthropic`, …) or `"all"`. */
  provider: string;
  /** Harness profile id, {@link UNKNOWN_HARNESS}, or `"all"`. */
  harness: string;
  /** ACP session id or `"all"`. */
  session: string;
  /** Resolved model id or `"all"`. */
  model: string;
  /** Failed or cancelled turns, or turns with failed / rejected tool calls. */
  problemsOnly: boolean;
  /** Turns whose prompt cache hit rate is below the low-cache threshold. */
  lowCacheOnly: boolean;
  /** Case-insensitive substring over ids, models, and prompt / output previews. */
  query: string;
}

export const EMPTY_TASK_FILTER: TaskFilter = {
  protocol: "all",
  provider: "all",
  harness: "all",
  session: "all",
  model: "all",
  problemsOnly: false,
  lowCacheOnly: false,
  query: "",
};

export interface TaskFilterContext {
  thresholdBps: number;
  outcomes: ReadonlyMap<string, ToolOutcomeCounts>;
}

export function taskHarness(task: TaskSnapshot): string {
  return task.harness_profile ?? UNKNOWN_HARNESS;
}

export function taskProvider(task: TaskSnapshot): string {
  return task.provider_id || "agy";
}

export function isTaskProblem(task: TaskSnapshot, tools?: ToolOutcomeCounts): boolean {
  if (task.phase === "failed" || task.phase === "cancelled") return true;
  return tools !== undefined && tools.failed + tools.rejected > 0;
}

export function isTaskLowCache(task: TaskSnapshot, thresholdBps: number): boolean {
  const bps = taskHitBps(task);
  return bps !== null && bps < thresholdBps;
}

export function isTaskFilterActive(filter: TaskFilter): boolean {
  return (
    filter.protocol !== "all" ||
    filter.provider !== "all" ||
    filter.harness !== "all" ||
    filter.session !== "all" ||
    filter.model !== "all" ||
    filter.problemsOnly ||
    filter.lowCacheOnly ||
    filter.query.trim() !== ""
  );
}

export function matchesTaskFilter(task: TaskSnapshot, filter: TaskFilter, ctx: TaskFilterContext): boolean {
  if (filter.protocol !== "all" && task.protocol !== filter.protocol) return false;
  if (filter.provider !== "all" && taskProvider(task) !== filter.provider) return false;
  if (filter.harness !== "all" && taskHarness(task) !== filter.harness) return false;
  if (filter.session !== "all" && task.session_id !== filter.session) return false;
  if (filter.model !== "all" && task.resolved_model !== filter.model) return false;
  if (filter.problemsOnly && !isTaskProblem(task, ctx.outcomes.get(task.task_id))) return false;
  if (filter.lowCacheOnly && !isTaskLowCache(task, ctx.thresholdBps)) return false;
  const query = filter.query.trim().toLowerCase();
  if (query !== "" && !matchesTaskQuery(task, query)) return false;
  return true;
}

/** `query` must already be trimmed and lower-cased. */
export function matchesTaskQuery(task: TaskSnapshot, query: string): boolean {
  return [
    task.task_id,
    task.provider_id,
    task.session_id,
    task.resolved_model,
    task.requested_model,
    task.prompt_preview,
    task.output_preview,
    task.error_message,
  ].some((field) => typeof field === "string" && field.toLowerCase().includes(query));
}

export function filterTasks(
  tasks: readonly TaskSnapshot[],
  filter: TaskFilter,
  ctx: TaskFilterContext,
): TaskSnapshot[] {
  if (!isTaskFilterActive(filter)) return [...tasks];
  return tasks.filter((task) => matchesTaskFilter(task, filter, ctx));
}

/** Counts per key in first-seen order (recent tasks arrive newest first). */
export function countInOrder<K>(
  tasks: readonly TaskSnapshot[],
  key: (task: TaskSnapshot) => K | null | undefined,
): Map<K, number> {
  const out = new Map<K, number>();
  for (const task of tasks) {
    const k = key(task);
    if (k === null || k === undefined) continue;
    out.set(k, (out.get(k) ?? 0) + 1);
  }
  return out;
}

export interface TaskSetSummary {
  count: number;
  /** Failed or cancelled turns, or turns with failed / rejected tool calls. */
  problems: number;
  /** Token-weighted cache hit rate across turns that reported usage. */
  hitBps: number | null;
  ttftP50Ms: number | null;
  ttftP90Ms: number | null;
  /** Sum over priced turns; `null` when none are priced. */
  costUsd: number | null;
  sessions: number;
}

export function summarizeTasks(
  tasks: readonly TaskSnapshot[],
  outcomes: ReadonlyMap<string, ToolOutcomeCounts>,
): TaskSetSummary {
  let problems = 0;
  let prompt = 0;
  let cached = 0;
  let costUsd: number | null = null;
  const ttfts: number[] = [];
  const sessions = new Set<string>();
  for (const task of tasks) {
    if (isTaskProblem(task, outcomes.get(task.task_id))) problems += 1;
    if (task.usage) {
      const p = taskPromptTokens(task);
      prompt += p;
      cached += Math.min(task.usage.cachedReadTokens, p);
    }
    if (typeof task.ttft_ms === "number") ttfts.push(task.ttft_ms);
    if (typeof task.cost_usd === "number") costUsd = (costUsd ?? 0) + task.cost_usd;
    if (task.session_id) sessions.add(task.session_id);
  }
  ttfts.sort((a, b) => a - b);
  return {
    count: tasks.length,
    problems,
    hitBps: prompt > 0 ? Math.round((cached * 10000) / prompt) : null,
    ttftP50Ms: percentile(ttfts, 0.5),
    ttftP90Ms: percentile(ttfts, 0.9),
    costUsd,
    sessions: sessions.size,
  };
}

const FILTER_STORAGE_KEY = "lg.overview.taskFilter";

/** Restores the filter saved for this tab, so it survives switching views. */
export function loadTaskFilter(): TaskFilter {
  try {
    const raw = window.sessionStorage.getItem(FILTER_STORAGE_KEY);
    if (!raw) return EMPTY_TASK_FILTER;
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return EMPTY_TASK_FILTER;
    const rec = parsed as Record<string, unknown>;
    const str = (key: keyof TaskFilter): string =>
      typeof rec[key] === "string" ? (rec[key] as string) : (EMPTY_TASK_FILTER[key] as string);
    const protocol = str("protocol");
    return {
      protocol: protocol === "openai_responses" || protocol === "anthropic_messages" ? protocol : "all",
      provider: str("provider") || "all",
      harness: str("harness"),
      session: str("session"),
      model: str("model"),
      problemsOnly: rec.problemsOnly === true,
      lowCacheOnly: rec.lowCacheOnly === true,
      query: str("query"),
    };
  } catch {
    return EMPTY_TASK_FILTER;
  }
}

export function saveTaskFilter(filter: TaskFilter): void {
  try {
    if (isTaskFilterActive(filter)) window.sessionStorage.setItem(FILTER_STORAGE_KEY, JSON.stringify(filter));
    else window.sessionStorage.removeItem(FILTER_STORAGE_KEY);
  } catch {
    // Storage may be unavailable (private mode, quota); the filter simply won't persist.
  }
}
