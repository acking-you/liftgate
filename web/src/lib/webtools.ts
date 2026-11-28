import { baseToolName, classifyTool } from "@/lib/toolcalls";

export type WebMode = "search" | "fetch";

/** Whether a tool searches the web or fetches a page; `null` for every non-web tool. */
export function webToolMode(name: string): WebMode | null {
  if (classifyTool(name) !== "web") return null;
  return baseToolName(name).toLowerCase().includes("search") ? "search" : "fetch";
}

export interface WebInput {
  query: string | null;
  url: string | null;
  /** What the harness asked the fetcher to extract or look for. */
  prompt: string | null;
  /** Remaining scalar arguments (domain filters, limits, …). */
  params: Array<[string, string]>;
}

const QUERY_KEYS = ["query", "q", "search_query", "search_term", "searchTerm", "keywords"];
const URL_KEYS = ["url", "uri", "href", "link"];
const PROMPT_KEYS = ["prompt", "pattern", "question", "instruction", "instructions"];

function parseObject(raw: string): Record<string, unknown> | null {
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
    if (typeof value === "string" && value.trim() !== "") return [key, value.trim()];
  }
  return null;
}

function scalarText(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value) && value.every((v) => typeof v === "string" || typeof v === "number")) {
    return value.join(", ");
  }
  return null;
}

/** Query / URL / prompt of a web tool call, or `null` when the arguments carry neither. */
export function parseWebInput(raw: string): WebInput | null {
  const obj = parseObject(raw);
  if (!obj) return null;
  const used = new Set<string>();
  let query = firstString(obj, QUERY_KEYS);
  if (!query && Array.isArray(obj.queries)) {
    const first = obj.queries.find((q): q is string => typeof q === "string" && q.trim() !== "");
    if (first) query = ["queries", obj.queries.filter((q) => typeof q === "string").join(" · ")];
  }
  const url = firstString(obj, URL_KEYS);
  const prompt = firstString(obj, PROMPT_KEYS);
  if (!query && !url) return null;
  for (const hit of [query, url, prompt]) if (hit) used.add(hit[0]);
  const params: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(obj)) {
    if (used.has(key)) continue;
    const text = scalarText(value);
    if (text !== null && text !== "") params.push([key, text]);
  }
  return { query: query?.[1] ?? null, url: url?.[1] ?? null, prompt: prompt?.[1] ?? null, params };
}

export interface WebSource {
  url: string;
  title: string | null;
  host: string;
  snippet: string | null;
}

export interface FetchedPage {
  url: string;
  title: string | null;
  mediaType: string | null;
  errorCode: string | null;
  body: string;
}

export interface WebOutput {
  /** Pages the gateway's own web fetch returned (`<gateway_web_fetch_result>`). */
  pages: FetchedPage[];
  /** Links / search hits found in the output, deduplicated, in order of appearance. */
  sources: WebSource[];
  /** Readable body once structured envelopes are lifted out. */
  body: string;
}

const MAX_SOURCES = 60;

/** Host name without `www.`, or `null` for anything that is not an http(s) URL. */
export function hostOf(url: string): string | null {
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
    return parsed.hostname.replace(/^www\./, "");
  } catch {
    return null;
  }
}

function cleanUrl(url: string): string {
  return url.replace(/[.,;:!?]+$/, "");
}

class SourceList {
  readonly items: WebSource[] = [];
  private readonly seen = new Map<string, WebSource>();

  add(rawUrl: string, title: string | null, snippet: string | null = null): void {
    const url = cleanUrl(rawUrl.trim());
    const host = hostOf(url);
    if (!host) return;
    const existing = this.seen.get(url);
    if (existing) {
      if (!existing.title && title) existing.title = title;
      if (!existing.snippet && snippet) existing.snippet = snippet;
      return;
    }
    if (this.items.length >= MAX_SOURCES) return;
    const source = { url, title: title?.trim() || null, host, snippet: snippet?.trim() || null };
    this.seen.set(url, source);
    this.items.push(source);
  }
}

function pickString(obj: Record<string, unknown>, keys: readonly string[]): string | null {
  for (const key of keys) {
    const value = obj[key];
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  return null;
}

function collectJson(value: unknown, out: SourceList, depth: number): void {
  if (depth > 5) return;
  if (Array.isArray(value)) {
    for (const item of value) collectJson(item, out, depth + 1);
    return;
  }
  if (value === null || typeof value !== "object") return;
  const obj = value as Record<string, unknown>;
  const url = pickString(obj, ["url", "link", "href", "uri"]);
  if (url) {
    out.add(
      url,
      pickString(obj, ["title", "name", "page_title"]),
      pickString(obj, ["snippet", "description", "content", "text", "summary"])?.slice(0, 280) ?? null,
    );
  }
  for (const child of Object.values(obj)) {
    if (child !== null && typeof child === "object") collectJson(child, out, depth + 1);
  }
}

function tryJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function attrsOf(raw: string): Map<string, string> {
  const attrs = new Map<string, string>();
  for (const match of raw.matchAll(/([\w:-]+)="([^"]*)"/g)) {
    const [, key, value] = match;
    if (key !== undefined && value !== undefined) attrs.set(key, value);
  }
  return attrs;
}

const FETCH_RESULT_RE = /<gateway_web_fetch_result\b([^>]*)>\n?([\s\S]*?)\n?<\/gateway_web_fetch_result>/g;
const JSON_ARRAY_RE = /\[\s*\{[\s\S]*?\}\s*\]/g;
const MD_LINK_RE = /\[([^\]\n]{1,300})\]\((https?:\/\/[^\s)]+)\)/g;
const BARE_URL_RE = /https?:\/\/[^\s<>"'`)\]}]+/g;

/** Lift search hits, fetched pages and links out of a web tool's text output. */
export function parseWebOutput(text: string): WebOutput {
  const sources = new SourceList();
  const pages: FetchedPage[] = [];
  let body = text;

  for (const match of text.matchAll(FETCH_RESULT_RE)) {
    const attrs = attrsOf(match[1] ?? "");
    const url = attrs.get("url") ?? "";
    pages.push({
      url,
      title: attrs.get("title") ?? null,
      mediaType: attrs.get("media_type") ?? null,
      errorCode: attrs.get("is_error") === "true" ? (attrs.get("error_code") ?? "error") : null,
      body: match[2] ?? "",
    });
  }
  if (pages.length > 0) body = pages.map((p) => p.body).join("\n\n");

  const whole = text.trim();
  if (pages.length === 0 && (whole.startsWith("{") || whole.startsWith("["))) {
    const parsed = tryJson(whole);
    if (parsed !== undefined) collectJson(parsed, sources, 0);
  }
  for (const match of body.matchAll(JSON_ARRAY_RE)) {
    const parsed = tryJson(match[0]);
    if (parsed !== undefined) collectJson(parsed, sources, 0);
  }
  for (const match of body.matchAll(MD_LINK_RE)) {
    if (match[2]) sources.add(match[2], match[1] ?? null);
  }
  for (const match of body.matchAll(BARE_URL_RE)) sources.add(match[0], null);
  const pageUrls = new Set(pages.map((p) => p.url));
  return { pages, sources: sources.items.filter((s) => !pageUrls.has(s.url)), body };
}
