import * as React from "react";
import { useMemo, useState } from "react";
import { Play, Sparkles } from "lucide-react";
import { CopyButton } from "@/components/CopyButton";
import { Button, Card, EmptyState, Pill, SearchInput, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { modelFamily } from "@/lib/stats";

type Family = "all" | "gemini" | "claude" | "other";

export function ModelsView({ onProbe }: { onProbe: (modelId: string) => void }): React.JSX.Element {
  const { snapshot } = useGateway();
  const models = useMemo(() => snapshot?.available_models ?? [], [snapshot?.available_models]);
  const defaultModel = snapshot?.acp_process.default_model;
  const [family, setFamily] = useState<Family>("all");
  const [query, setQuery] = useState("");

  const counts = useMemo(() => {
    const out = { gemini: 0, claude: 0, other: 0 };
    for (const m of models) out[modelFamily(m.modelId)] += 1;
    return out;
  }, [models]);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return models.filter((m) => {
      if (family !== "all" && modelFamily(m.modelId) !== family) return false;
      return q === "" || m.modelId.toLowerCase().includes(q) || m.name.toLowerCase().includes(q);
    });
  }, [models, family, query]);

  return (
    <Card
      flush
      title={`模型目录 · ${models.length}`}
    >
      <div className="lg-toolbar">
        <Segmented<Family>
          ariaLabel="按模型系列筛选"
          value={family}
          onChange={setFamily}
          options={[
            { value: "all", label: "全部", count: models.length },
            { value: "gemini", label: "Gemini", count: counts.gemini },
            { value: "claude", label: "Claude", count: counts.claude },
            { value: "other", label: "其他", count: counts.other },
          ]}
        />
        <SearchInput value={query} onChange={setQuery} placeholder="搜索模型 ID 或名称" className="w-full sm:w-64" />
      </div>
      {shown.length === 0 ? (
        <EmptyState
          icon={<Sparkles />}
          title={models.length === 0 ? "还没有模型" : "没有匹配的模型"}
          hint={models.length === 0 ? "网关完成与 agy acp 的首次握手后，模型目录会出现在这里。" : "换个关键词试试。"}
        />
      ) : (
        <div className="lg-table-wrap">
          <table className="lg-table">
            <thead>
              <tr>
                <th>模型 ID</th>
                <th className="hide-sm">名称</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {shown.map((m) => (
                <tr key={m.modelId}>
                  <td>
                    <span className="inline-flex items-center gap-1.5">
                      <span className="lg-num text-[12.5px] font-medium text-fg">{m.modelId}</span>
                      <CopyButton value={m.modelId} title="复制模型 ID" />
                      {m.modelId === defaultModel && <Pill tone="accent">默认</Pill>}
                    </span>
                  </td>
                  <td className="hide-sm text-fg-3">{m.name || "—"}</td>
                  <td className="text-right">
                    <Button size="sm" onClick={() => onProbe(m.modelId)}>
                      <Play />
                      拨测
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}
