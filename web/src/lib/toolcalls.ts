import { percentile } from "@/lib/format";
import { sortHarnessIds } from "@/lib/harness";
import { msg } from "@/lib/i18n";
import {
  type CodeLanguage,
  guessScriptLanguage,
  normalizeLanguage,
} from "@/lib/highlight";
import type { ToolCallRecord, ToolCallRoute } from "@/types/telemetry";

export type ToolKind = "orchestrator" | "patch" | "shell" | "file" | "search" | "web" | "other";

export function toolKindLabel(kind: ToolKind): string {
  return msg().tools.kind[kind];
}

export function routeLabel(route: ToolCallRoute): string {
  return msg().tools.route[route];
}

export type RouteGroup = "client" | "unwrapped" | "local";

export function routeGroup(route: ToolCallRoute): RouteGroup {
  switch (route) {
    case "client_roundtrip":
      return "client";
    case "namespace_hub":
    case "generic_dispatcher":
      return "unwrapped";
    default:
      return "local";
  }
}

/** Strip `mcp__server__` / `mcp_lg_ns__` style prefixes down to the bare tool name. */
export function baseToolName(name: string): string {
  const idx = name.lastIndexOf("__");
  return idx >= 0 && idx + 2 < name.length ? name.slice(idx + 2) : name;
}

const ORCHESTRATOR_NAMES = new Set(["exec", "execute", "eval", "repl", "js_repl"]);
const PATCH_NAMES = new Set([
  "apply_patch",
  "patch",
  "edit",
  "multiedit",
  "str_replace",
  "str_replace_editor",
  "str_replace_based_edit_tool",
]);
const SHELL_NAMES = new Set([
  "exec_command",
  "bash",
  "shell",
  "local_shell",
  "run_command",
  "terminal",
  "write_stdin",
]);
const FILE_NAMES = new Set([
  "read",
  "read_file",
  "write",
  "write_file",
  "glob",
  "grep",
  "ls",
  "list",
  "list_dir",
  "view_image",
  "notebookedit",
]);

export function classifyTool(name: string, isOrchestrator = false): ToolKind {
  if (isOrchestrator) return "orchestrator";
  const base = baseToolName(name).toLowerCase();
  if (ORCHESTRATOR_NAMES.has(base)) return "orchestrator";
  if (PATCH_NAMES.has(base)) return "patch";
  if (SHELL_NAMES.has(base)) return "shell";
  if (FILE_NAMES.has(base)) return "file";
  if (base.includes("search_client_tools") || base === "tool_search" || base === "toolsearch") {
    return "search";
  }
  if (base.includes("web") || base.includes("fetch")) return "web";
  if (base.includes("context_spill")) return "file";
  return "other";
}

export function displayToolName(rec: ToolCallRecord): string {
  return rec.client_tool_name ?? rec.wire_tool_name;
}

export function recordKind(rec: ToolCallRecord): ToolKind {
  return classifyTool(displayToolName(rec), rec.is_orchestrator);
}

export function isFailure(rec: ToolCallRecord): boolean {
  return rec.status === "error" || rec.status === "preflight_rejected";
}

/** How a turn's tool calls ended, for the at-a-glance pills beside each request. */
export interface ToolOutcomeCounts {
  total: number;
  /** Errors, script exceptions, and calls whose result never came back. */
  failed: number;
  /** Rejected by gateway preflight or aimed at a disabled tool. */
  rejected: number;
  /** Still waiting for the client's result. */
  waiting: number;
}

export function toolOutcomesByTask(
  records: readonly ToolCallRecord[],
): ReadonlyMap<string, ToolOutcomeCounts> {
  const out = new Map<string, ToolOutcomeCounts>();
  for (const rec of records) {
    const counts = out.get(rec.task_id) ?? { total: 0, failed: 0, rejected: 0, waiting: 0 };
    counts.total += 1;
    if (rec.status === "error" || rec.status === "abandoned") counts.failed += 1;
    else if (rec.status === "preflight_rejected") counts.rejected += 1;
    else if (rec.status === "pending") counts.waiting += 1;
    out.set(rec.task_id, counts);
  }
  return out;
}

/* ---------- input rendering ---------- */

export type ParsedToolInput =
  | { view: "code"; language: CodeLanguage; code: string; params: Array<[string, string]> }
  | { view: "patch"; patch: string; params: Array<[string, string]> }
  | { view: "json"; pretty: string }
  | { view: "text"; text: string };

function tryParseObject(raw: string): Record<string, unknown> | null {
  const trimmed = raw.trim();
  if (!trimmed.startsWith("{")) return null;
  try {
    const parsed: unknown = JSON.parse(trimmed);
    return parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function firstString(obj: Record<string, unknown>, keys: readonly string[]): [string, string] | null {
  for (const key of keys) {
    const value = obj[key];
    if (typeof value === "string" && value.length > 0) return [key, value];
    if (Array.isArray(value) && value.length > 0 && value.every((v) => typeof v === "string")) {
      return [key, (value as string[]).join(" ")];
    }
  }
  return null;
}

function scalarParams(obj: Record<string, unknown>, skip: ReadonlySet<string>): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(obj)) {
    if (skip.has(key)) continue;
    if (value === null || value === undefined) continue;
    out.push([key, typeof value === "string" ? value : JSON.stringify(value)]);
  }
  return out;
}

export function prettyJson(raw: string): string | null {
  const trimmed = raw.trim();
  if (!(trimmed.startsWith("{") || trimmed.startsWith("["))) return null;
  try {
    return JSON.stringify(JSON.parse(trimmed), null, 2);
  } catch {
    return null;
  }
}

function looksLikePatch(text: string): boolean {
  return /^\*\*\* Begin Patch|^diff --git |^@@ |^\*\*\* (?:Update|Add|Delete) File:/m.test(text);
}

const CODE_KEYS = ["code", "script", "source", "input", "js", "py"] as const;
const PATCH_KEYS = ["patch", "patchText", "patch_text", "diff", "input"] as const;
const COMMAND_KEYS = ["command", "cmd", "script"] as const;
const LANG_KEYS = ["language", "lang", "runtime"] as const;
const FILE_KEYS = ["file_path", "filePath", "path", "file"] as const;

function synthesizeEditPatch(obj: Record<string, unknown>): string | null {
  const file = firstString(obj, FILE_KEYS)?.[1] ?? "(unnamed)";
  const pairs: Array<[string, string]> = [];
  const pushPair = (src: Record<string, unknown>): void => {
    const before = firstString(src, ["old_string", "oldString", "old_str", "old_text"]);
    const after = src.new_string ?? src.newString ?? src.new_str ?? src.new_text;
    if (before && typeof after === "string") pairs.push([before[1], after]);
  };
  pushPair(obj);
  if (Array.isArray(obj.edits)) {
    for (const edit of obj.edits) {
      if (edit !== null && typeof edit === "object") pushPair(edit as Record<string, unknown>);
    }
  }
  if (pairs.length === 0) return null;
  const body = pairs
    .map(
      ([before, after]) =>
        `@@\n${before
          .split("\n")
          .map((l) => `-${l}`)
          .join("\n")}\n${after
          .split("\n")
          .map((l) => `+${l}`)
          .join("\n")}`,
    )
    .join("\n");
  return `*** Update File: ${file}\n${body}`;
}

/**
 * Turn raw tool arguments into the most readable view: CodeMode scripts and
 * shell commands come out of their JSON string escaping, patches become diffs.
 */
export function parseToolInput(
  toolName: string,
  raw: string,
  hints: { isOrchestrator?: boolean; language?: string | null } = {},
): ParsedToolInput {
  const kind = classifyTool(toolName, hints.isOrchestrator);
  const obj = tryParseObject(raw);
  const hinted = normalizeLanguage(hints.language);

  if (kind === "orchestrator") {
    if (obj) {
      const code = firstString(obj, CODE_KEYS);
      if (code) {
        const lang =
          hinted ?? normalizeLanguage(firstString(obj, LANG_KEYS)?.[1]) ?? guessScriptLanguage(code[1]);
        return {
          view: "code",
          language: lang,
          code: code[1],
          params: scalarParams(obj, new Set([code[0]])),
        };
      }
    } else if (raw.trim().length > 0) {
      return {
        view: "code",
        language: hinted ?? guessScriptLanguage(raw),
        code: raw,
        params: [],
      };
    }
  }

  if (kind === "patch") {
    if (obj) {
      const patch = firstString(obj, PATCH_KEYS);
      if (patch && looksLikePatch(patch[1])) {
        return { view: "patch", patch: patch[1], params: scalarParams(obj, new Set([patch[0]])) };
      }
      const synthesized = synthesizeEditPatch(obj);
      if (synthesized) {
        return {
          view: "patch",
          patch: synthesized,
          params: scalarParams(
            obj,
            new Set([
              ...FILE_KEYS,
              "old_string",
              "oldString",
              "old_str",
              "old_text",
              "new_string",
              "newString",
              "new_str",
              "new_text",
              "edits",
            ]),
          ),
        };
      }
    } else if (looksLikePatch(raw)) {
      return { view: "patch", patch: raw, params: [] };
    }
  }

  if (kind === "shell" && obj) {
    const command = firstString(obj, COMMAND_KEYS);
    if (command) {
      return {
        view: "code",
        language: "sh",
        code: command[1],
        params: scalarParams(obj, new Set([command[0]])),
      };
    }
  }

  const pretty = prettyJson(raw);
  if (pretty !== null) return { view: "json", pretty };
  if (looksLikePatch(raw)) return { view: "patch", patch: raw, params: [] };
  return { view: "text", text: raw };
}

/* ---------- patch parsing ---------- */

export type PatchLineKind = "meta" | "file" | "hunk" | "add" | "del" | "ctx";

export interface PatchLine {
  kind: PatchLineKind;
  text: string;
}

export interface ParsedPatch {
  lines: PatchLine[];
  files: number;
  added: number;
  removed: number;
}

export function parsePatch(text: string): ParsedPatch {
  const lines: PatchLine[] = [];
  let files = 0;
  let added = 0;
  let removed = 0;
  for (const line of text.split("\n")) {
    if (/^\*\*\* (?:Begin|End) Patch/.test(line) || line.startsWith("*** End of File")) {
      lines.push({ kind: "meta", text: line });
    } else if (/^\*\*\* (?:Update|Add|Delete) File:/.test(line) || line.startsWith("diff --git ")) {
      files += 1;
      lines.push({ kind: "file", text: line });
    } else if (
      line.startsWith("*** Move to:") ||
      /^(?:---|\+\+\+) (?:[ab]\/|\/dev\/null)/.test(line) ||
      line.startsWith("index ")
    ) {
      lines.push({ kind: "file", text: line });
    } else if (line.startsWith("@@")) {
      lines.push({ kind: "hunk", text: line });
    } else if (line.startsWith("+")) {
      added += 1;
      lines.push({ kind: "add", text: line });
    } else if (line.startsWith("-")) {
      removed += 1;
      lines.push({ kind: "del", text: line });
    } else {
      lines.push({ kind: "ctx", text: line });
    }
  }
  return { lines, files, added, removed };
}

/* ---------- filtering & aggregation ---------- */

export interface ToolCallFilter {
  query: string;
  harness: string;
  kind: ToolKind | "all";
  session: string;
  errorsOnly: boolean;
  spilledOnly: boolean;
}

export const EMPTY_FILTER: ToolCallFilter = {
  query: "",
  harness: "all",
  kind: "all",
  session: "all",
  errorsOnly: false,
  spilledOnly: false,
};

export function isFilterActive(f: ToolCallFilter): boolean {
  return (
    f.query.trim() !== "" ||
    f.harness !== "all" ||
    f.kind !== "all" ||
    f.session !== "all" ||
    f.errorsOnly ||
    f.spilledOnly
  );
}

export function filterToolCalls(records: ToolCallRecord[], f: ToolCallFilter): ToolCallRecord[] {
  const q = f.query.trim().toLowerCase();
  return records.filter((rec) => {
    if (f.errorsOnly && !isFailure(rec)) return false;
    if (f.spilledOnly && !rec.output_spilled) return false;
    if (f.harness !== "all" && rec.harness_profile !== f.harness) return false;
    if (f.session !== "all" && rec.session_id !== f.session) return false;
    if (f.kind !== "all" && recordKind(rec) !== f.kind) return false;
    if (q === "") return true;
    return (
      rec.wire_tool_name.toLowerCase().includes(q) ||
      (rec.client_tool_name?.toLowerCase().includes(q) ?? false) ||
      rec.input_preview.toLowerCase().includes(q) ||
      rec.output_preview.toLowerCase().includes(q) ||
      (rec.session_id?.toLowerCase().includes(q) ?? false) ||
      rec.task_id.toLowerCase().includes(q)
    );
  });
}

export interface HarnessStats {
  harness: string;
  calls: number;
  sessions: number;
  failures: number;
  failureRate: number;
  orchestratorCalls: number;
  orchestratorShare: number;
  localCalls: number;
  spilled: number;
  avgInputBytes: number;
  avgOutputBytes: number;
  p50DurationMs: number | null;
  p95DurationMs: number | null;
}

export function aggregateByHarness(records: ToolCallRecord[]): HarnessStats[] {
  const buckets = new Map<string, ToolCallRecord[]>();
  for (const rec of records) {
    const list = buckets.get(rec.harness_profile);
    if (list) list.push(rec);
    else buckets.set(rec.harness_profile, [rec]);
  }
  return sortHarnessIds(buckets.keys()).map((harness) => {
    const list = buckets.get(harness) ?? [];
    const calls = list.length;
    let failures = 0;
    let orchestratorCalls = 0;
    let localCalls = 0;
    let spilled = 0;
    let inputBytes = 0;
    let outputBytes = 0;
    const sessions = new Set<string>();
    const durations: number[] = [];
    for (const rec of list) {
      if (isFailure(rec)) failures += 1;
      if (recordKind(rec) === "orchestrator") orchestratorCalls += 1;
      if (routeGroup(rec.route) === "local") localCalls += 1;
      if (rec.output_spilled) spilled += 1;
      inputBytes += rec.input_bytes;
      outputBytes += rec.output_bytes;
      if (rec.session_id) sessions.add(rec.session_id);
      if (typeof rec.duration_ms === "number") durations.push(rec.duration_ms);
    }
    durations.sort((a, b) => a - b);
    return {
      harness,
      calls,
      sessions: sessions.size,
      failures,
      failureRate: calls > 0 ? failures / calls : 0,
      orchestratorCalls,
      orchestratorShare: calls > 0 ? orchestratorCalls / calls : 0,
      localCalls,
      spilled,
      avgInputBytes: calls > 0 ? inputBytes / calls : 0,
      avgOutputBytes: calls > 0 ? outputBytes / calls : 0,
      p50DurationMs: percentile(durations, 0.5),
      p95DurationMs: percentile(durations, 0.95),
    };
  });
}

export function countBy<T>(items: T[], key: (item: T) => string | null): Array<[string, number]> {
  const counts = new Map<string, number>();
  for (const item of items) {
    const k = key(item);
    if (k === null) continue;
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  return [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
}

/* ---------- script anatomy ---------- */

export interface NestedToolUse {
  name: string;
  /** 0-based line numbers where the script invokes the tool. */
  lines: number[];
}

/** Client tools a CodeMode script reaches through its `tools.*` bindings. */
export function nestedToolUses(code: string): NestedToolUse[] {
  const found = new Map<string, number[]>();
  const pattern = /\btools\s*(?:\.\s*([A-Za-z_$][\w$]*)|\[\s*["']([^"']+)["']\s*\])/g;
  code.split("\n").forEach((line, idx) => {
    for (const match of line.matchAll(pattern)) {
      const name = match[1] ?? match[2];
      if (!name) continue;
      const lines = found.get(name);
      if (!lines) found.set(name, [idx]);
      else if (lines[lines.length - 1] !== idx) lines.push(idx);
    }
  });
  return [...found.entries()]
    .map(([name, lines]) => ({ name, lines }))
    .sort((a, b) => b.lines.length - a.lines.length || a.name.localeCompare(b.name));
}

export interface OutputEnvelope {
  /** `Script completed` / `Script failed` line of a CodeMode orchestrator result. */
  script: "completed" | "failed" | null;
  exitCode: number | null;
  wallSeconds: number | null;
  originalTokens: number | null;
  /** Set when the harness itself clipped the output before returning it. */
  truncated: boolean;
  totalLines: number | null;
  chunkId: string | null;
  body: string;
}

/**
 * Harness tool results lead with a small status header before `Output:`
 * (Codex `exec`: `Script completed` + `Wall time`; `exec_command`: `Process
 * exited with code`). Lift it out so the body reads like a terminal, and so a
 * failed script is visible even when the protocol-level error flag is unset.
 */
export function parseOutputEnvelope(text: string): OutputEnvelope | null {
  const marker = /^Output:[ \t]*\n?/m.exec(text);
  if (!marker || marker.index > 400) return null;
  const head = text.slice(0, marker.index);
  const script = /^Script (completed|failed)/m.exec(head);
  const exit = /^Process exited with code (-?\d+)/m.exec(head);
  const wall = /^Wall time:? ([\d.]+) seconds/m.exec(head);
  if (!script && !exit && !wall) return null;
  const chunk = /^Chunk ID: (\S+)/m.exec(head);
  let body = text.slice(marker.index + marker[0].length).replace(/^\n/, "");
  let tokens = /^Original token count: (\d+)/m.exec(head)?.[1];
  let truncated = false;
  let totalLines: number | null = null;
  const clipped = /^Warning: truncated output \(original token count: (\d+)\)\n(?:Total output lines: (\d+)\n)?\n?/.exec(body);
  if (clipped) {
    truncated = true;
    tokens = clipped[1] ?? tokens;
    totalLines = clipped[2] !== undefined ? Number.parseInt(clipped[2], 10) : null;
    body = body.slice(clipped[0].length);
  }
  return {
    script: script?.[1] === "completed" ? "completed" : script?.[1] === "failed" ? "failed" : null,
    exitCode: exit?.[1] !== undefined ? Number.parseInt(exit[1], 10) : null,
    wallSeconds: wall?.[1] !== undefined ? Number.parseFloat(wall[1]) : null,
    originalTokens: tokens !== undefined ? Number.parseInt(tokens, 10) : null,
    truncated,
    totalLines,
    chunkId: chunk?.[1] ?? null,
    body,
  };
}

/** True when the output itself reports failure, whatever the error flag says. */
export function outputReportsFailure(text: string): boolean {
  const envelope = parseOutputEnvelope(text);
  if (!envelope) return false;
  return envelope.script === "failed" || (envelope.exitCode !== null && envelope.exitCode !== 0);
}

export interface OutputSection {
  title: string;
  /** 0-based line of the `=== title ===` marker. */
  line: number;
}

/** `=== title ===` rulers that scripts print between the things they collected. */
export function outputSections(body: string): OutputSection[] {
  const sections: OutputSection[] = [];
  body.split("\n").forEach((line, idx) => {
    const match = /^=== (.{1,80}?) ===\s*$/.exec(line);
    if (match?.[1]) sections.push({ title: match[1], line: idx });
  });
  return sections;
}

export interface PatchFile {
  path: string;
  op: "update" | "add" | "delete";
  /** Index of the file header inside `ParsedPatch.lines`. */
  line: number;
  added: number;
  removed: number;
}

export function patchFiles(parsed: ParsedPatch): PatchFile[] {
  const files: PatchFile[] = [];
  parsed.lines.forEach((line, idx) => {
    if (line.kind === "file") {
      const v4a = /^\*\*\* (Update|Add|Delete) File:\s*(.+)$/.exec(line.text);
      const git = /^diff --git a\/(\S+)/.exec(line.text);
      if (v4a?.[1] && v4a[2]) {
        const op = v4a[1] === "Add" ? "add" : v4a[1] === "Delete" ? "delete" : "update";
        files.push({ path: v4a[2].trim(), op, line: idx, added: 0, removed: 0 });
      } else if (git?.[1]) {
        files.push({ path: git[1], op: "update", line: idx, added: 0, removed: 0 });
      }
      return;
    }
    const current = files[files.length - 1];
    if (!current) return;
    if (line.kind === "add") current.added += 1;
    else if (line.kind === "del") current.removed += 1;
  });
  return files;
}

export function errorKindLabel(kind: string | null | undefined): string {
  const labels = msg().tools.errorKind;
  switch (kind) {
    case "tool_error":
    case "script_exception":
    case "preflight_rejected":
    case "disabled_tool":
      return labels[kind];
    case "unknown":
    case null:
    case undefined:
      return labels.failed;
    default:
      return kind;
  }
}
