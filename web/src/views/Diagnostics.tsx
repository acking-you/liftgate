import * as React from "react";
import { useMemo } from "react";
import { CircleCheck } from "lucide-react";
import { CodeBlock } from "@/components/CodeBlock";
import { Card, Note } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { formatClock } from "@/lib/format";
import { useI18n } from "@/lib/i18n";

export function DiagnosticsView(): React.JSX.Element {
  const { client, snapshot, apiKey } = useGateway();
  const { m } = useI18n();
  const t = m.infra.diagnostics;
  const launchers: ReadonlyArray<readonly [command: string, target: string]> = [
    ["lg cx", "Codex CLI"],
    ["lg cxa", t.codexDesktop],
    ["lg cc", "Claude Code"],
    ["lg cd", t.claudeDesktop],
    ["lg oc", "OpenCode"],
    ["lg omp", "Oh My Pi"],
  ];
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
        title={t.stderr(stderr.length)}
      >
        {stderr.length === 0 ? (
          <Note tone="ok" icon={<CircleCheck />}>
            {t.clean}
          </Note>
        ) : (
          <CodeBlock code={log} language="text" title="stderr" maxHeight={420} />
        )}
      </Card>

      <Card
        title={t.harness}
        sub={t.harnessSub}
      >
        <ul className="m-0 grid list-none grid-cols-2 gap-2 p-0 sm:grid-cols-3 xl:grid-cols-6">
          {launchers.map(([command, target]) => (
            <li key={command} className="rounded-lg border border-line px-3 py-2.5">
              <code className="lg-num text-[13px] font-semibold text-fg">{command}</code>
              <div className="mt-0.5 text-[12px] text-fg-3">{target}</div>
            </li>
          ))}
        </ul>
      </Card>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <Card title={t.manualCodex}>
          <CodeBlock code={codexSnippet} language="sh" title="config.toml" maxHeight={null} />
        </Card>
        <Card title={t.manualClaude}>
          <CodeBlock code={claudeSnippet} language="sh" title={t.env} maxHeight={null} />
        </Card>
      </div>
    </>
  );
}
