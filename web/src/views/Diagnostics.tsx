import * as React from "react";
import { useMemo } from "react";
import { CircleCheck } from "lucide-react";
import { CodeBlock } from "@/components/CodeBlock";
import { Card, Note } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { formatClock } from "@/lib/format";

const LAUNCHERS: ReadonlyArray<readonly [command: string, target: string]> = [
  ["lg cx", "Codex CLI"],
  ["lg cxa", "Codex 桌面端"],
  ["lg cc", "Claude Code"],
  ["lg cd", "Claude 桌面端"],
  ["lg oc", "OpenCode"],
  ["lg omp", "Oh My Pi"],
];

export function DiagnosticsView(): React.JSX.Element {
  const { client, snapshot, apiKey } = useGateway();
  const acp = snapshot?.acp_process;
  const stderr = acp?.recent_stderr ?? [];
  const model = acp?.default_model ?? "gemini-4-carbon-high";
  const origin = client.origin;

  const log = useMemo(
    () => stderr.map((e) => `[${formatClock(e.timestamp_unix_ms)}] ${e.line}`).join("\n"),
    [stderr],
  );

  const codexSnippet = `# ~/.codex/config.toml
model = "${model}"
model_provider = "agy"
model_catalog_json = "~/.codex/agy-models.json"
web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "${origin}/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"`;

  const claudeSnippet = `# Linux / macOS
export ANTHROPIC_BASE_URL="${origin}"
export ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
export ANTHROPIC_MODEL="${model}"
claude

# Windows PowerShell
$env:ANTHROPIC_BASE_URL="${origin}"
$env:ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
$env:ANTHROPIC_MODEL="${model}"
claude`;

  return (
    <>
      <Card
        title={`agy acp stderr · ${stderr.length} 行`}
      >
        {stderr.length === 0 ? (
          <Note tone="ok" icon={<CircleCheck />}>
            stderr 是干净的，没有告警输出。
          </Note>
        ) : (
          <CodeBlock code={log} language="text" title="stderr" maxHeight={420} />
        )}
      </Card>

      <Card
        title="接入 Harness"
        sub="lg 启动器只把网关配置注入子进程，退出后还原，不改全局配置。"
      >
        <ul className="m-0 grid list-none grid-cols-2 gap-2 p-0 sm:grid-cols-3 xl:grid-cols-6">
          {LAUNCHERS.map(([command, target]) => (
            <li key={command} className="rounded-lg border border-line px-3 py-2.5">
              <code className="lg-num text-[13px] font-semibold text-fg">{command}</code>
              <div className="mt-0.5 text-[12px] text-fg-3">{target}</div>
            </li>
          ))}
        </ul>
      </Card>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card title="手动接入 Codex CLI">
          <CodeBlock code={codexSnippet} language="sh" title="config.toml" maxHeight={null} />
        </Card>
        <Card title="手动接入 Claude Code">
          <CodeBlock code={claudeSnippet} language="sh" title="环境变量" maxHeight={null} />
        </Card>
      </div>
    </>
  );
}
