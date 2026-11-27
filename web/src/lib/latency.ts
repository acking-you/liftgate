import type { Tone } from "@/components/ui";

/** Upper bounds (ms) of the green, neutral, and amber bands; anything slower is red. */
export interface LatencyScale {
  readonly ok: number;
  readonly neutral: number;
  readonly warn: number;
}

export type LatencyKind = "ttft" | "elapsed" | "tool";

/**
 * Time to first token: thinking models routinely need a few seconds, so only waits past ten
 * seconds stand out.
 */
export const TTFT_SCALE: LatencyScale = { ok: 3_000, neutral: 10_000, warn: 30_000 };
/** Whole request: long generations are normal, so only multi-minute turns are flagged. */
export const ELAPSED_SCALE: LatencyScale = { ok: 15_000, neutral: 60_000, warn: 180_000 };
/** Tool round trip: includes the client running the tool. */
export const TOOL_SCALE: LatencyScale = { ok: 2_000, neutral: 30_000, warn: 120_000 };

export const LATENCY_SCALES: Record<LatencyKind, LatencyScale> = {
  ttft: TTFT_SCALE,
  elapsed: ELAPSED_SCALE,
  tool: TOOL_SCALE,
};

export type LatencyTone = Extract<Tone, "ok" | "neutral" | "warn" | "err">;

export function latencyTone(ms: number, scale: LatencyScale): LatencyTone {
  if (ms < scale.ok) return "ok";
  if (ms < scale.neutral) return "neutral";
  if (ms < scale.warn) return "warn";
  return "err";
}
