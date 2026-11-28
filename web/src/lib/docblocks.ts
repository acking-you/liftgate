/**
 * Harnesses inline documents into prompts as tagged blocks: `<environment_context>`,
 * `<system-reminder>`, `<file path="…">`, `<INSTRUCTIONS>`, `<image name=[Image #1]>`, …
 * Splitting them back out lets the timeline show each document on its own.
 */
export type DocSegment =
  | { kind: "text"; text: string }
  | { kind: "block"; tag: string; attrs: Array<[string, string]>; body: string };

/** Opening tag at the start of a line: `<tag>` / `<tag attr="v">` / `<permissions instructions>`. */
const OPEN_RE = /^[ \t]*<([A-Za-z_][\w.:-]*)((?:[ \t]+[^<>\n]*?)?)[ \t]*>/gm;
/** Texts beyond this size are shown raw rather than scanned. */
const MAX_SCAN_CHARS = 4 * 1024 * 1024;

function parseAttrs(raw: string): Array<[string, string]> {
  const attrs: Array<[string, string]> = [];
  for (const match of raw.matchAll(/([\w.:-]+)(?:=(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g)) {
    const key = match[1];
    if (!key) continue;
    attrs.push([key, match[2] ?? match[3] ?? match[4] ?? ""]);
  }
  return attrs;
}

/** Index just past the close tag balancing an opening `<tag>` whose body starts at `from`. */
function findClose(text: string, tag: string, inner: string, from: number): { start: number; end: number } | null {
  const closes = inner !== tag ? [`</${inner}>`, `</${tag}>`] : [`</${tag}>`];
  for (const close of closes) {
    const openRe = new RegExp(`<${tag.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?=[\\s>])`, "g");
    let depth = 1;
    let cursor = from;
    while (cursor < text.length) {
      const closeAt = text.indexOf(close, cursor);
      if (closeAt < 0) break;
      openRe.lastIndex = cursor;
      let nested = openRe.exec(text);
      while (nested && nested.index < closeAt) {
        depth += 1;
        nested = openRe.exec(text);
      }
      depth -= 1;
      if (depth === 0) return { start: closeAt, end: closeAt + close.length };
      cursor = closeAt + close.length;
    }
  }
  return null;
}

/** Top-level tagged documents in `text`, or `null` when there are none worth splitting. */
export function splitDocBlocks(text: string): DocSegment[] | null {
  if (text.length > MAX_SCAN_CHARS || !text.includes("</")) return null;
  const segments: DocSegment[] = [];
  let cursor = 0;
  OPEN_RE.lastIndex = 0;
  let match = OPEN_RE.exec(text);
  while (match) {
    if (match.index < cursor) {
      match = OPEN_RE.exec(text);
      continue;
    }
    const tag = match[1] ?? "";
    const attrRaw = (match[2] ?? "").trim();
    const inner = attrRaw ? `${tag} ${attrRaw}` : tag;
    const bodyStart = match.index + match[0].length;
    const close = findClose(text, tag, inner, bodyStart);
    if (!close) {
      match = OPEN_RE.exec(text);
      continue;
    }
    const before = text.slice(cursor, match.index);
    if (before.trim()) segments.push({ kind: "text", text: before.replace(/^\n+|\s+$/g, "") });
    segments.push({
      kind: "block",
      tag,
      attrs: parseAttrs(attrRaw),
      body: text.slice(bodyStart, close.start).replace(/^\n/, "").replace(/\n[ \t]*$/, ""),
    });
    cursor = close.end;
    OPEN_RE.lastIndex = cursor;
    match = OPEN_RE.exec(text);
  }
  if (!segments.some((s) => s.kind === "block")) return null;
  const tail = text.slice(cursor);
  if (tail.trim()) segments.push({ kind: "text", text: tail.replace(/^\n+|\s+$/g, "") });
  return segments;
}

/** The attribute that best names a block (`path`, `name`, `url`, …), for its heading. */
export function blockLabel(attrs: ReadonlyArray<readonly [string, string]>): string | null {
  for (const key of ["path", "file_path", "file", "name", "url", "title", "id", "source"]) {
    const hit = attrs.find(([k, v]) => k === key && v !== "");
    if (hit) return hit[1];
  }
  return null;
}
