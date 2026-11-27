import { taskHitBps } from "@/lib/format";
import type { ToolOutcomeCounts } from "@/lib/toolcalls";
import type { TaskProtocol, TaskSnapshot } from "@/types/telemetry";

/** Filter key for turns whose harness is not (yet) attributed. */
export const UNKNOWN_HARNESS = "unknown";

export interface TaskFilter {
  protocol: "all" | TaskProtocol;
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
}

export const EMPTY_TASK_FILTER: TaskFilter = {
  protocol: "all",
  harness: "all",
  session: "all",
  model: "all",
  problemsOnly: false,
  lowCacheOnly: false,
};

export interface TaskFilterContext {
  thresholdBps: number;
  outcomes: ReadonlyMap<string, ToolOutcomeCounts>;
}

export function taskHarness(task: TaskSnapshot): string {
  return task.harness_profile ?? UNKNOWN_HARNESS;
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
    filter.harness !== "all" ||
    filter.session !== "all" ||
    filter.model !== "all" ||
    filter.problemsOnly ||
    filter.lowCacheOnly
  );
}

export function matchesTaskFilter(task: TaskSnapshot, filter: TaskFilter, ctx: TaskFilterContext): boolean {
  if (filter.protocol !== "all" && task.protocol !== filter.protocol) return false;
  if (filter.harness !== "all" && taskHarness(task) !== filter.harness) return false;
  if (filter.session !== "all" && task.session_id !== filter.session) return false;
  if (filter.model !== "all" && task.resolved_model !== filter.model) return false;
  if (filter.problemsOnly && !isTaskProblem(task, ctx.outcomes.get(task.task_id))) return false;
  if (filter.lowCacheOnly && !isTaskLowCache(task, ctx.thresholdBps)) return false;
  return true;
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
