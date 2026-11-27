import * as React from "react";
import { useMemo, useState } from "react";
import { Image as ImageIcon } from "lucide-react";
import { CodeBlock, ProseBlock } from "@/components/CodeBlock";
import { RankedBars } from "@/components/charts";
import { Disclosure, SearchInput, Tag } from "@/components/ui";
import { formatBytes } from "@/lib/format";
import { useI18n } from "@/lib/i18n";
import { prettyJson } from "@/lib/toolcalls";
import type { CapturedAcpPromptBlock, CapturedToolSpec } from "@/types/telemetry";

/** What the gateway actually handed to `agy acp` for one turn. */
export function AcpRequestView({
  systemAppend,
  toolsDelta,
  promptBlocks,
  promptExcerpt,
  excerptOnly = false,
}: {
  systemAppend?: string | null;
  toolsDelta?: string | null;
  /** Full `session/prompt` blocks when the payload was saved. */
  promptBlocks?: CapturedAcpPromptBlock[] | null;
  /** Truncated fallback when only the record survived. */
  promptExcerpt?: string | null;
  excerptOnly?: boolean;
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.payload;
  const hasPrompt = (promptBlocks?.length ?? 0) > 0 || Boolean(promptExcerpt);
  return (
    <div className="flex min-w-0 flex-col gap-3">
      {excerptOnly && (
        <p className="lg-hint m-0">{t.excerptOnly}</p>
      )}
      {systemAppend && (
        <Disclosure
          head={
            <>
              <span className="flex-none font-medium text-fg-2">{t.bridge}</span>
              <span className="lg-num lg-clip min-w-0 text-[11.5px] text-fg-4">
                _meta.antigravity.systemPrompt.append
              </span>
            </>
          }
          meta={formatBytes(new TextEncoder().encode(systemAppend).length)}
        >
          <CodeBlock code={systemAppend} language="xml" title="systemPrompt.append" wrap />
        </Disclosure>
      )}
      {toolsDelta && (
        <Disclosure
          head={
            <>
              <span className="flex-none font-medium text-fg-2">{t.toolsDelta}</span>
              <span className="lg-num lg-clip min-w-0 text-[11.5px] text-fg-4">&lt;client_tools_update&gt;</span>
            </>
          }
          meta={formatBytes(new TextEncoder().encode(toolsDelta).length)}
        >
          <CodeBlock code={toolsDelta} language="xml" title="<client_tools_update>" wrap />
        </Disclosure>
      )}
      {promptBlocks && promptBlocks.length > 0
        ? promptBlocks.map((block, idx) =>
            // Prompt blocks are an ordered, immutable list.
            block.type === "text" ? (
              <CodeBlock
                key={idx}
                code={block.text}
                language="xml"
                title={t.textBlock(idx + 1)}
                wrap
                maxHeight={520}
              />
            ) : (
              <div key={idx} className="flex items-center gap-2 rounded-lg border border-line p-3 text-[12.5px] text-fg-3">
                <ImageIcon className="h-4 w-4" />
                <span className="lg-num">
                  {t.imageBlock(idx + 1, block.mime_type, formatBytes(block.base64_bytes))}
                </span>
              </div>
            ),
          )
        : promptExcerpt && <ProseBlock text={promptExcerpt} title={t.excerpt} />}
      {!systemAppend && !toolsDelta && !hasPrompt && (
        <p className="lg-hint m-0">{t.notSaved}</p>
      )}
    </div>
  );
}

/** Client-declared tools of one turn, heaviest schema first. */
export function ClientToolsView({
  tools,
  fallbackNames,
}: {
  tools: CapturedToolSpec[];
  /** Names only, when the full payload is unavailable. */
  fallbackNames?: string[];
}): React.JSX.Element {
  const { m } = useI18n();
  const t = m.context.payload;
  const [query, setQuery] = useState("");
  const sorted = useMemo(() => [...tools].sort((a, b) => b.schema_bytes - a.schema_bytes), [tools]);
  const total = useMemo(() => tools.reduce((sum, t) => sum + t.schema_bytes, 0), [tools]);
  const q = query.trim().toLowerCase();
  const shown = q === "" ? sorted : sorted.filter((t) => t.name.toLowerCase().includes(q));

  if (tools.length === 0) {
    if (fallbackNames && fallbackNames.length > 0) {
      return (
        <div className="flex flex-col gap-2">
          <p className="lg-hint m-0">{t.namesOnly(fallbackNames.length)}</p>
          <div className="flex flex-wrap gap-1.5">
            {fallbackNames.map((name) => (
              <Tag key={name}>{name}</Tag>
            ))}
          </div>
        </div>
      );
    }
    return <p className="lg-hint m-0">{t.noTools}</p>;
  }

  return (
    <div className="flex min-w-0 flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="lg-hint m-0">
          {t.toolsSummary(tools.length, <span className="lg-num text-fg-2">{formatBytes(total)}</span>)}
        </p>
        {tools.length > 10 && (
          <SearchInput value={query} onChange={setQuery} placeholder={t.filter} className="w-52" />
        )}
      </div>
      {q === "" && sorted.length > 3 && (
        <div className="lg-well">
          <RankedBars
            rows={sorted.slice(0, 8).map((t) => ({
              key: t.name,
              label: <span className="lg-num">{t.name}</span>,
              value: t.schema_bytes,
              display: formatBytes(t.schema_bytes),
              hint: t.name,
            }))}
          />
        </div>
      )}
      {shown.map((tool) => (
        <Disclosure
          key={tool.name}
          head={
            <>
              <span className="lg-num font-medium text-fg">{tool.name}</span>
              <Tag>{tool.kind}</Tag>
              <span className="lg-clip min-w-0 flex-1 text-[11.5px] text-fg-4">
                {tool.description?.slice(0, 120)}
              </span>
            </>
          }
          meta={formatBytes(tool.schema_bytes)}
        >
          <div className="flex flex-col gap-2">
            {tool.description && <ProseBlock text={tool.description} title="Description" maxHeight={260} />}
            {tool.schema_or_format && (
              <CodeBlock
                code={prettyJson(tool.schema_or_format) ?? tool.schema_or_format}
                language={prettyJson(tool.schema_or_format) !== null ? "json" : "text"}
                title={tool.kind === "custom" ? "Custom Grammar" : "Input Schema"}
                maxHeight={320}
              />
            )}
          </div>
        </Disclosure>
      ))}
    </div>
  );
}
