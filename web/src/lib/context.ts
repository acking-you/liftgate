import type { SegmentPart } from "@/components/charts";
import type {
  CapturedFullMessage,
  ContextSegment,
  ContextSegmentKey,
  LowCacheRequestMetrics,
} from "@/types/telemetry";

export const SEGMENT_LABEL: Record<ContextSegmentKey, string> = {
  wire_tools: "Wire 工具定义",
  bridge_rules: "桥接规则提示词",
  orchestrator_contract: "编排器契约",
  client_tools_update: "增量工具同步",
  developer_instructions: "开发者指令",
  user_text: "用户文本",
  assistant_text: "助手文本",
  reasoning: "思考",
  tool_calls: "工具调用",
  tool_results: "工具输出",
  images: "图像",
};

export interface NamedBytes {
  key: string;
  label: string;
  bytes: number;
}

type GroupId = "tooling" | "instructions" | "dialogue" | "roundtrip" | "images";

const GROUP_OF: Record<ContextSegmentKey, GroupId> = {
  wire_tools: "tooling",
  bridge_rules: "tooling",
  orchestrator_contract: "tooling",
  client_tools_update: "tooling",
  developer_instructions: "instructions",
  user_text: "dialogue",
  assistant_text: "dialogue",
  reasoning: "dialogue",
  tool_calls: "roundtrip",
  tool_results: "roundtrip",
  images: "images",
};

/** Fixed group order and colours: identity never moves when a group is empty. */
const GROUPS: ReadonlyArray<{ id: GroupId; label: string; color: string; ink?: "dark" }> = [
  { id: "tooling", label: "工具定义与契约", color: "var(--lg-s1)" },
  { id: "instructions", label: "开发者指令", color: "var(--lg-s2)" },
  { id: "dialogue", label: "对话文本", color: "var(--lg-s3)" },
  { id: "roundtrip", label: "工具往返", color: "var(--lg-s4)", ink: "dark" },
  { id: "images", label: "图像", color: "var(--lg-s5)" },
];

export function groupSegments(
  segments: ReadonlyArray<{ key: ContextSegmentKey; bytes: number }>,
): SegmentPart[] {
  const totals = new Map<GroupId, number>();
  for (const seg of segments) {
    const group = GROUP_OF[seg.key];
    totals.set(group, (totals.get(group) ?? 0) + seg.bytes);
  }
  return GROUPS.map((g) => ({
    key: g.id,
    label: g.label,
    value: totals.get(g.id) ?? 0,
    color: g.color,
    ink: g.ink,
  }));
}

export function rankSegments(segments: ReadonlyArray<ContextSegment>): NamedBytes[] {
  return segments
    .filter((s) => s.bytes > 0)
    .map((s) => ({ key: s.key, label: SEGMENT_LABEL[s.key], bytes: s.bytes }))
    .sort((a, b) => b.bytes - a.bytes);
}

/** Composition of a low-cache turn from the metrics the gateway already records. */
export function segmentsFromMetrics(m: LowCacheRequestMetrics): ContextSegment[] {
  const seg = (key: ContextSegmentKey, bytes: number): ContextSegment => ({
    key,
    bytes,
    tokens_est: Math.round(bytes / 4),
  });
  return [
    seg("bridge_rules", m.acp_system_append_bytes),
    seg("client_tools_update", m.tools_delta_xml_bytes),
    seg("developer_instructions", m.developer_instructions_bytes),
    seg("user_text", m.user_text_bytes),
    seg("assistant_text", m.assistant_text_bytes),
    seg("reasoning", m.reasoning_bytes),
    seg("tool_calls", m.tool_call_bytes),
    seg("tool_results", m.tool_result_bytes),
    seg("images", m.image_bytes),
  ];
}

export type PartKindId = "text" | "instructions" | "reasoning" | "tool_call" | "tool_result" | "image";

export const PART_KIND_LABEL: Record<PartKindId, string> = {
  text: "文本",
  instructions: "开发者指令",
  reasoning: "思考",
  tool_call: "工具调用",
  tool_result: "工具输出",
  image: "图像",
};

const PART_KIND_STYLE: Record<PartKindId, { color: string; ink?: "dark" }> = {
  text: { color: "var(--lg-s3)" },
  instructions: { color: "var(--lg-s2)" },
  reasoning: { color: "var(--lg-s-neutral)" },
  tool_call: { color: "var(--lg-s1)" },
  tool_result: { color: "var(--lg-s4)", ink: "dark" },
  image: { color: "var(--lg-s5)" },
};

export function partKindColor(kind: PartKindId): string {
  return PART_KIND_STYLE[kind].color;
}

export function partKindOf(part: CapturedFullMessage["parts"][number]): PartKindId {
  if (part.kind === "text") return part.is_developer_instructions ? "instructions" : "text";
  return part.kind;
}

export function partBytes(part: CapturedFullMessage["parts"][number]): number {
  return part.kind === "image" ? part.base64_bytes : part.bytes;
}

const PART_KIND_ORDER: readonly PartKindId[] = [
  "instructions",
  "text",
  "reasoning",
  "tool_call",
  "tool_result",
  "image",
];

export function messagePartComposition(messages: CapturedFullMessage[]): SegmentPart[] {
  const totals = new Map<PartKindId, number>();
  for (const msg of messages) {
    for (const part of msg.parts) {
      const kind = partKindOf(part);
      totals.set(kind, (totals.get(kind) ?? 0) + partBytes(part));
    }
  }
  return PART_KIND_ORDER.map((kind) => ({
    key: kind,
    label: PART_KIND_LABEL[kind],
    value: totals.get(kind) ?? 0,
    ...PART_KIND_STYLE[kind],
  }));
}
