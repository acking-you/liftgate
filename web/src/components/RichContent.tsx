import * as React from "react";
import { useMemo, useState } from "react";
import {
  Download,
  ExternalLink,
  FileText,
  Globe,
  Image as ImageIcon,
  Link2,
  Maximize2,
  Search,
} from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { Disclosure, Pill, Segmented } from "@/components/ui";
import { Modal } from "@/components/ui/dialog";
import { type DocSegment, blockLabel, splitDocBlocks } from "@/lib/docblocks";
import { formatBytes } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { decodedBytes, imageDataUrl, imageExtension } from "@/lib/media";
import { type FetchedPage, type WebInput, type WebMode, type WebSource, hostOf, parseWebOutput } from "@/lib/webtools";

/* ---------- Images ---------- */

/**
 * A captured image: a zoomable thumbnail when the gateway kept its pixels, otherwise a frame
 * drawn at its aspect ratio (older captures and images over the preview cap carry no pixels).
 */
export function ImagePreview({
  mediaType,
  data,
  base64Bytes,
  width,
  height,
  label,
}: {
  mediaType: string;
  data?: string | null;
  base64Bytes: number;
  width?: number | null;
  height?: number | null;
  label?: string;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.media;
  const url = useMemo(() => imageDataUrl(mediaType, data), [mediaType, data]);
  const [natural, setNatural] = useState<readonly [number, number] | null>(null);
  const [zoom, setZoom] = useState(false);
  const w = width ?? natural?.[0] ?? null;
  const h = height ?? natural?.[1] ?? null;
  const title = label ?? t.image;
  const dims = w && h ? `${w} × ${h}` : null;

  const meta = (
    <div className="min-w-0 text-[12.5px]">
      <div className="flex items-center gap-1.5 font-medium text-fg">
        <ImageIcon className="h-3.5 w-3.5 flex-none text-fg-3" />
        <span className="lg-clip">{title}</span>
      </div>
      <div className="lg-num mt-0.5 text-[12px] text-fg-3">
        {mediaType}
        {dims ? ` · ${dims}` : ""}
      </div>
      <div className="lg-num text-[12px] text-fg-3">
        {t.decoded(formatBytes(decodedBytes(base64Bytes)))} · base64 {formatBytes(base64Bytes)}
      </div>
      {url ? (
        <div className="mt-1.5 flex flex-wrap items-center gap-3">
          <button type="button" className="lg-link" onClick={() => setZoom(true)}>
            <Maximize2 />
            {t.zoom}
          </button>
          <a className="lg-link" href={url} download={`liftgate-image.${imageExtension(mediaType)}`}>
            <Download />
            {t.download}
          </a>
        </div>
      ) : (
        <p className="lg-hint m-0 mt-1">{t.noData}</p>
      )}
    </div>
  );

  if (!url) {
    const ratio = w && h ? w / h : 4 / 3;
    const boxH = 96;
    const boxW = Math.max(56, Math.min(220, Math.round(boxH * ratio)));
    return (
      <div className="lg-media">
        <div className="lg-imgframe" style={{ width: boxW, height: boxH }}>
          <ImageIcon />
        </div>
        {meta}
      </div>
    );
  }
  return (
    <div className="lg-media">
      <button type="button" className="lg-media-thumb" title={t.zoom} onClick={() => setZoom(true)}>
        <img
          src={url}
          alt={title}
          loading="lazy"
          decoding="async"
          onLoad={(ev) => setNatural([ev.currentTarget.naturalWidth, ev.currentTarget.naturalHeight])}
        />
      </button>
      {meta}
      <Modal
        open={zoom}
        onOpenChange={setZoom}
        title={title}
        description={`${mediaType}${dims ? ` · ${dims}` : ""} · ${formatBytes(decodedBytes(base64Bytes))}`}
        className="lg-dialog-media"
      >
        <div className="lg-media-full">
          <img src={url} alt={title} />
        </div>
      </Modal>
    </div>
  );
}

/* ---------- Web search / fetch ---------- */

function safeHref(url: string): string | undefined {
  return hostOf(url) ? url : undefined;
}

function UrlLine({ url, title }: { url: string; title?: string | null }): React.JSX.Element {
  const host = hostOf(url);
  const href = safeHref(url);
  const content = (
    <>
      <span className="flex-none font-medium text-fg">{title || host || url}</span>
      <span className="lg-clip lg-num min-w-0 text-fg-3">{title ? `${host ?? ""} · ${url}` : url}</span>
      {href && <ExternalLink />}
    </>
  );
  return href ? (
    <a className="lg-web-url" href={href} target="_blank" rel="noopener noreferrer" title={url}>
      {content}
    </a>
  ) : (
    <span className="lg-web-url">{content}</span>
  );
}

/** What a web tool was asked: the search query, or the page it fetched and what to pull out of it. */
export function WebInputCard({ mode, input }: { mode: WebMode; input: WebInput }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.web;
  return (
    <div className="lg-web-card">
      <span className="lg-web-icon" aria-hidden="true">
        {mode === "search" ? <Search /> : <Globe />}
      </span>
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <span className="text-[11.5px] text-fg-3">{mode === "search" ? t.searchLabel : t.fetchLabel}</span>
        {input.query && <span className="lg-web-query">{input.query}</span>}
        {input.url && <UrlLine url={input.url} />}
        {input.prompt && (
          <p className="m-0 text-[12.5px] text-fg-2">
            <span className="text-fg-3">{t.prompt}</span> {input.prompt}
          </p>
        )}
      </div>
    </div>
  );
}

const SOURCE_PREVIEW = 9;

function SourceGrid({ sources }: { sources: WebSource[] }): React.JSX.Element {
  const { m } = useI18n();
  const [all, setAll] = useState(false);
  const shown = all ? sources : sources.slice(0, SOURCE_PREVIEW);
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <div className="lg-web-sources">
        {shown.map((src) => (
          <a
            key={src.url}
            className="lg-web-source"
            href={src.url}
            target="_blank"
            rel="noopener noreferrer"
            title={src.url}
          >
            <span className="lg-web-fav" aria-hidden="true">
              {src.host.slice(0, 1).toUpperCase()}
            </span>
            <span className="flex min-w-0 flex-col">
              <span className="lg-clip text-[12.5px] font-medium text-fg">{src.title ?? src.host}</span>
              <span className="lg-clip lg-num text-[11.5px] text-fg-3">{src.host}</span>
              {src.snippet && <span className="mt-0.5 line-clamp-2 text-[11.5px] text-fg-3">{src.snippet}</span>}
            </span>
          </a>
        ))}
      </div>
      {sources.length > SOURCE_PREVIEW && (
        <button type="button" className="lg-link self-start" onClick={() => setAll((v) => !v)}>
          {all ? m.tools.web.fewer : m.tools.web.more(sources.length - SOURCE_PREVIEW)}
        </button>
      )}
    </div>
  );
}

function PageCard({ page, maxHeight }: { page: FetchedPage; maxHeight: number | null }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.web;
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <div className="lg-web-card">
        <span className="lg-web-icon" aria-hidden="true">
          <Globe />
        </span>
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <UrlLine url={page.url} title={page.title} />
          <span className="flex flex-wrap items-center gap-1.5">
            {page.mediaType && <Pill>{page.mediaType}</Pill>}
            {page.errorCode ? <Pill tone="err">{t.fetchError(page.errorCode)}</Pill> : <Pill tone="ok">{t.fetched}</Pill>}
          </span>
        </div>
      </div>
      <ProseBlock
        text={page.body}
        title={page.errorCode ? t.errorBody : t.content}
        tone={page.errorCode ? "err" : undefined}
        maxHeight={maxHeight}
      />
    </div>
  );
}

/**
 * A web tool's result as a reader would want it: search hits as source cards, fetched pages
 * with their URL and title, and the readable body. The plain rendering is one click away.
 */
export function WebOutputView({
  mode,
  text,
  failed,
  maxHeight,
  fallback,
}: {
  mode: WebMode;
  text: string;
  failed: boolean;
  maxHeight: number | null;
  /** The generic rendering, shown when the reader asks for the raw output. */
  fallback: React.ReactNode;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.tools.web;
  const parsed = useMemo(() => parseWebOutput(text), [text]);
  const [raw, setRaw] = useState(false);
  const structured = parsed.pages.length > 0 || parsed.sources.length > 0;
  if (!structured) return <>{fallback}</>;

  const bar = (
    <div className="flex flex-wrap items-center justify-between gap-2">
      <span className="flex flex-wrap items-center gap-1.5">
        {parsed.pages.length > 0 && <Pill>{t.pages(parsed.pages.length)}</Pill>}
        {parsed.sources.length > 0 && (
          <Pill>{mode === "search" ? t.sources(parsed.sources.length) : t.links(parsed.sources.length)}</Pill>
        )}
      </span>
      <button
        type="button"
        className="lg-btn"
        data-size="sm"
        data-variant="ghost"
        aria-pressed={raw}
        onClick={() => setRaw((v) => !v)}
      >
        {t.raw}
      </button>
    </div>
  );
  if (raw) {
    return (
      <div className="flex min-w-0 flex-col gap-2">
        {bar}
        {fallback}
      </div>
    );
  }
  const linkList =
    parsed.sources.length > 0 &&
    (mode === "search" && parsed.pages.length === 0 ? (
      <SourceGrid sources={parsed.sources} />
    ) : (
      <Disclosure
        head={
          <>
            <Link2 className="h-3.5 w-3.5 flex-none text-fg-3" />
            <span className="font-medium text-fg-2">{t.links(parsed.sources.length)}</span>
          </>
        }
      >
        <SourceGrid sources={parsed.sources} />
      </Disclosure>
    ));
  return (
    <div className="flex min-w-0 flex-col gap-2">
      {bar}
      {mode === "search" && parsed.pages.length === 0 && linkList}
      {parsed.pages.length > 0
        ? parsed.pages.map((page, idx) => (
            // Pages keep the order the gateway fetched them in.
            <PageCard key={`${idx}-${page.url}`} page={page} maxHeight={maxHeight} />
          ))
        : parsed.body.trim() !== "" && (
            <ProseBlock
              text={parsed.body}
              title={mode === "search" ? t.results : t.content}
              tone={failed ? "err" : undefined}
              maxHeight={maxHeight}
            />
          )}
      {!(mode === "search" && parsed.pages.length === 0) && linkList}
    </div>
  );
}

/* ---------- Documents inlined into text ---------- */

function DocSegments({ segments, depth }: { segments: DocSegment[]; depth: number }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      {segments.map((seg, idx) => {
        // Segments are an ordered, immutable split of one text part.
        if (seg.kind === "text") {
          return <ProseBlock key={idx} text={seg.text} title={t.docText} maxHeight={360} />;
        }
        const label = blockLabel(seg.attrs);
        const nested = depth < 2 ? splitDocBlocks(seg.body) : null;
        const imageLike = seg.tag.toLowerCase().includes("image");
        return (
          <Disclosure
            key={idx}
            defaultOpen={segments.length === 1}
            meta={formatBytes(new TextEncoder().encode(seg.body).length)}
            head={
              <>
                {imageLike ? (
                  <ImageIcon className="h-3.5 w-3.5 flex-none text-fg-3" />
                ) : (
                  <FileText className="h-3.5 w-3.5 flex-none text-fg-3" />
                )}
                <span className="lg-num flex-none font-medium text-fg-2">{`<${seg.tag}>`}</span>
                {label && <span className="lg-clip lg-num min-w-0 text-[11.5px] text-fg">{label}</span>}
                {nested && <Pill>{t.docBlocks(nested.filter((s) => s.kind === "block").length)}</Pill>}
              </>
            }
          >
            {nested ? (
              <DocSegments segments={nested} depth={depth + 1} />
            ) : seg.body.trim() === "" ? (
              <p className="lg-hint m-0">{t.docEmpty}</p>
            ) : (
              <ProseBlock text={seg.body} title={`<${seg.tag}>`} maxHeight={360} />
            )}
          </Disclosure>
        );
      })}
    </div>
  );
}

/**
 * A text part with the documents harnesses inline into it (`<environment_context>`,
 * `<system-reminder>`, `<file …>`, …) split out into their own collapsible cards.
 */
export function TextPartView({ text, title }: { text: string; title: string }): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.timeline;
  const segments = useMemo(() => splitDocBlocks(text), [text]);
  const [mode, setMode] = useState<"doc" | "raw">("doc");
  if (!segments) return <ProseBlock text={text} title={title} />;
  const blocks = segments.filter((s) => s.kind === "block").length;
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="lg-num text-[12px] text-fg-3">
          {title} · {t.docBlocks(blocks)}
        </span>
        <Segmented<"doc" | "raw">
          ariaLabel={t.docViewAria}
          value={mode}
          onChange={setMode}
          options={[
            { value: "doc", label: t.docStructured },
            { value: "raw", label: t.docRaw },
          ]}
        />
      </div>
      {mode === "raw" ? <ProseBlock text={text} title={title} /> : <DocSegments segments={segments} depth={0} />}
    </div>
  );
}
