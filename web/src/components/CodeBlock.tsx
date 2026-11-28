import * as React from "react";
import { useEffect, useMemo, useRef, useState } from "react";
import { WrapText } from "lucide-react";
import { CopyButton } from "@/components/CopyButton";
import { byteLength, formatBytes } from "@/lib/format";
import { type CodeLanguage, tokenizeLines } from "@/lib/highlight";
import { msg, useI18n } from "@/lib/i18n";
import { parsePatch, patchFiles } from "@/lib/toolcalls";

/** Phones wrap by default: sideways scrolling inside a sheet is miserable. */
function prefersWrap(): boolean {
  return window.matchMedia("(max-width: 640px)").matches;
}

/** Lines rendered before the reader has to ask for the rest. */
const COLLAPSED_LINES = 160;

function languageLabel(language: CodeLanguage): string {
  switch (language) {
    case "js":
      return "JavaScript";
    case "py":
      return "Python";
    case "sh":
      return "Shell";
    case "json":
      return "JSON";
    case "xml":
      return "XML";
    case "text":
      return msg().common.text;
  }
}

interface FrameProps {
  title?: React.ReactNode;
  meta?: React.ReactNode;
  copyValue: string;
  tone?: "err";
  wrap: boolean;
  onToggleWrap: () => void;
  maxHeight: number | null;
  hiddenLines: number;
  onExpand: () => void;
  /** Rendered between the caption and the code, e.g. a patch's file index. */
  lead?: React.ReactNode;
  preRef?: React.Ref<HTMLPreElement>;
  children: React.ReactNode;
}

function Frame({
  title,
  meta,
  copyValue,
  tone,
  wrap,
  onToggleWrap,
  maxHeight,
  hiddenLines,
  onExpand,
  lead,
  preRef,
  children,
}: FrameProps): React.JSX.Element {
  const { m } = useI18n();
  return (
    <figure className="lg-codeblock m-0" data-tone={tone}>
      <figcaption className="lg-codeblock-head">
        <span className="flex min-w-0 items-center gap-2 truncate">{title}</span>
        <span className="flex flex-none items-center gap-1">
          {meta !== undefined && <span className="lg-num mr-1">{meta}</span>}
          <button
            type="button"
            className="lg-btn"
            data-size="sm"
            data-variant="ghost"
            data-icon="true"
            aria-pressed={wrap}
            title={m.common.wrap}
            aria-label={m.common.wrap}
            onClick={onToggleWrap}
          >
            <WrapText />
          </button>
          <CopyButton value={copyValue} />
        </span>
      </figcaption>
      {lead}
      <pre
        ref={preRef}
        className="lg-code"
        data-wrap={wrap}
        style={maxHeight === null ? undefined : { maxHeight }}
        tabIndex={0}
      >
        <code className="lg-code-lines">{children}</code>
      </pre>
      {hiddenLines > 0 && (
        <button type="button" className="lg-codeblock-more" onClick={onExpand}>
          {m.common.showMoreLines(hiddenLines.toLocaleString("en-US"))}
        </button>
      )}
    </figure>
  );
}

/** Scroll a row into view inside its own scroller (or the enclosing container when unclipped). */
function revealRow(pre: HTMLPreElement | null, selector: string): void {
  const row = pre?.querySelector<HTMLElement>(selector);
  if (!pre || !row) return;
  // `.lg-code-row` uses `display: contents` so it generates no layout box
  // (`offsetTop === 0`, empty rect); measure its first rendered grid cell.
  const rect = row.getBoundingClientRect();
  const anchor = rect.height > 0 ? row : row.firstElementChild;
  if (!(anchor instanceof HTMLElement)) return;
  const anchorRect = anchor.getBoundingClientRect();
  const preRect = pre.getBoundingClientRect();
  if (pre.scrollHeight > pre.clientHeight + 1) {
    const lead = Math.min(pre.clientHeight * 0.2, 64);
    const target = pre.scrollTop + (anchorRect.top - preRect.top) - lead;
    pre.scrollTo({ top: Math.max(0, target), behavior: "smooth" });
    if (preRect.bottom < 80 || preRect.top > window.innerHeight - 80) {
      pre.scrollIntoView({ block: "nearest", behavior: "smooth" });
    }
  } else {
    anchor.scrollIntoView({ block: "center", behavior: "smooth" });
  }
}

/** Source or payload text with line numbers, light syntax tinting, wrap and copy. */
export function CodeBlock({
  code,
  language = "text",
  title,
  meta,
  maxHeight = 440,
  wrap: initialWrap = false,
  tone,
  highlightLines,
  scrollLine,
  scrollNonce,
}: {
  code: string;
  language?: CodeLanguage;
  title?: React.ReactNode;
  meta?: React.ReactNode;
  /** Pixel cap for the scroll area; `null` lets the block grow. */
  maxHeight?: number | null;
  wrap?: boolean;
  /** Marks failing output (stderr, tool errors) with a red frame. */
  tone?: "err";
  /** 0-based lines to mark; the first one is scrolled into view. */
  highlightLines?: ReadonlySet<number>;
  /** Explicit 0-based line to scroll into view (overrides the first highlighted line). */
  scrollLine?: number | null;
  /** Increment to re-scroll to `scrollLine` even when its line number is unchanged. */
  scrollNonce?: number;
}): React.JSX.Element {
  const [wrap, setWrap] = useState(() => initialWrap || prefersWrap());
  const [expanded, setExpanded] = useState(false);
  const preRef = useRef<HTMLPreElement | null>(null);
  const lines = useMemo(() => tokenizeLines(code, language), [code, language]);

  const firstMarked = useMemo(
    () => (highlightLines && highlightLines.size > 0 ? Math.min(...highlightLines) : null),
    [highlightLines],
  );
  const targetLine = scrollLine ?? firstMarked;
  const showAll = expanded || (targetLine !== null && targetLine >= COLLAPSED_LINES);
  const visible = showAll ? lines : lines.slice(0, COLLAPSED_LINES);

  useEffect(() => {
    if (targetLine !== null && targetLine >= COLLAPSED_LINES) setExpanded(true);
  }, [targetLine]);

  useEffect(() => {
    if (targetLine !== null) revealRow(preRef.current, `[data-line="${targetLine}"]`);
  }, [targetLine, scrollNonce, highlightLines]);

  return (
    <Frame
      title={title ?? languageLabel(language)}
      meta={meta ?? `${msg().common.lines(lines.length.toLocaleString("en-US"))} · ${formatBytes(byteLength(code))}`}
      copyValue={code}
      tone={tone}
      wrap={wrap}
      onToggleWrap={() => setWrap((w) => !w)}
      maxHeight={maxHeight}
      hiddenLines={lines.length - visible.length}
      onExpand={() => setExpanded(true)}
      preRef={preRef}
    >
      {visible.map((tokens, idx) => (
        // Line order is the identity here: the block is immutable text.
        <span
          className="lg-code-row"
          data-line={idx}
          data-hl={highlightLines?.has(idx) ? "true" : undefined}
          key={idx}
        >
          <span className="lg-code-ln">{idx + 1}</span>
          <span className="lg-code-text">
            {tokens.map((token, tIdx) =>
              token.kind === "" ? (
                token.text
              ) : (
                <span key={tIdx} className={`tok-${token.kind}`}>
                  {token.text}
                </span>
              ),
            )}
          </span>
        </span>
      ))}
    </Frame>
  );
}

/** Five squares split between additions and deletions, the way code review tools draw it. */
export function DiffStat({ added, removed }: { added: number; removed: number }): React.JSX.Element {
  const total = added + removed;
  const green = total === 0 ? 0 : Math.max(added > 0 ? 1 : 0, Math.round((added / total) * 5));
  const red = total === 0 ? 0 : Math.min(5 - green, Math.max(removed > 0 ? 1 : 0, 5 - green));
  return (
    <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
      <span className="lg-num text-ok">+{added}</span>
      <span className="lg-num text-err">−{removed}</span>
      <span className="inline-flex gap-px" aria-hidden="true">
        {[0, 1, 2, 3, 4].map((i) => (
          <span
            key={i}
            className="lg-diffsq"
            data-kind={i < green ? "add" : i < green + red ? "del" : undefined}
          />
        ))}
      </span>
    </span>
  );
}


/** V4A / unified patches as a readable diff with a clickable per-file index. */
export function PatchBlock({
  patch,
  title,
  maxHeight = 440,
}: {
  patch: string;
  title?: React.ReactNode;
  maxHeight?: number | null;
}): React.JSX.Element {
  const [wrap, setWrap] = useState(prefersWrap);
  const [expanded, setExpanded] = useState(false);
  const [focus, setFocus] = useState<{ line: number; tick: number } | null>(null);
  const focusLine = focus?.line ?? null;
  const preRef = useRef<HTMLPreElement | null>(null);
  const parsed = useMemo(() => parsePatch(patch), [patch]);
  const files = useMemo(() => patchFiles(parsed), [parsed]);
  const showAll = expanded || (focusLine !== null && focusLine >= COLLAPSED_LINES);
  const visible = showAll ? parsed.lines : parsed.lines.slice(0, COLLAPSED_LINES);

  useEffect(() => {
    if (focusLine !== null && focusLine >= COLLAPSED_LINES) setExpanded(true);
  }, [focusLine]);

  useEffect(() => {
    if (focus !== null) revealRow(preRef.current, `[data-line="${focus.line}"]`);
  }, [focus]);

  return (
    <Frame
      title={title ?? msg().common.patch}
      meta={<DiffStat added={parsed.added} removed={parsed.removed} />}
      copyValue={patch}
      wrap={wrap}
      onToggleWrap={() => setWrap((w) => !w)}
      maxHeight={maxHeight}
      hiddenLines={parsed.lines.length - visible.length}
      onExpand={() => setExpanded(true)}
      preRef={preRef}
      lead={
        files.length > 0 ? (
          <ul className="lg-filelist">
            {files.map((file) => (
              <li key={`${file.line}-${file.path}`}>
                <button
                  type="button"
                  className="lg-filelist-item"
                  aria-pressed={focusLine === file.line}
                  onClick={() => setFocus((cur) => ({ line: file.line, tick: (cur?.tick ?? 0) + 1 }))}
                >
                  <span className="lg-fileop" data-op={file.op}>
                    {msg().common.patchOps[file.op]}
                  </span>
                  <span className="lg-num min-w-0 flex-1 truncate text-left" title={file.path}>
                    {file.path}
                  </span>
                  <DiffStat added={file.added} removed={file.removed} />
                </button>
              </li>
            ))}
          </ul>
        ) : undefined
      }
    >
      {visible.map((line, idx) => (
        <span
          className="lg-code-row"
          data-kind={line.kind}
          data-line={idx}
          data-hl={focusLine === idx ? "true" : undefined}
          key={idx}
        >
          <span className="lg-code-ln">{idx + 1}</span>
          <span className="lg-code-text">{line.text}</span>
        </span>
      ))}
    </Frame>
  );
}

/** Prose (instructions, reasoning, plain tool output): wrapped, clipped until asked for. */
export function ProseBlock({
  text,
  title,
  tone,
  clipChars = 6000,
  maxHeight = 440,
}: {
  text: string;
  title?: React.ReactNode;
  tone?: "err";
  clipChars?: number;
  maxHeight?: number | null;
}): React.JSX.Element {
  const [expanded, setExpanded] = useState(false);
  const clipped = !expanded && text.length > clipChars;
  return (
    <figure className="lg-codeblock m-0" data-tone={tone}>
      <figcaption className="lg-codeblock-head">
        <span className="flex min-w-0 items-center gap-2 truncate">{title ?? msg().common.text}</span>
        <span className="flex flex-none items-center gap-1">
          <span className="lg-num mr-1">{formatBytes(byteLength(text))}</span>
          <CopyButton value={text} />
        </span>
      </figcaption>
      <pre
        className="lg-code lg-code-plain"
        style={maxHeight === null ? undefined : { maxHeight }}
        tabIndex={0}
      >
        {clipped ? text.slice(0, clipChars) : text}
      </pre>
      {clipped && (
        <button type="button" className="lg-codeblock-more" onClick={() => setExpanded(true)}>
          {msg().common.showMoreChars((text.length - clipChars).toLocaleString("en-US"))}
        </button>
      )}
    </figure>
  );
}
