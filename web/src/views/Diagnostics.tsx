import * as React from "react";
import { useMemo, useState } from "react";
import { CircleAlert, CircleCheck, Play } from "lucide-react";
import { CodeBlock } from "@/components/CodeBlock";
import { Button, Card, Note, Pill, ProviderBadge } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import { formatClock } from "@/lib/format";
import { useI18n } from "@/lib/i18n";

export function DiagnosticsView(): React.JSX.Element {
  const { client, snapshot, apiKey, refresh } = useGateway();
  const { m } = useI18n();
  const t = m.infra.diagnostics;
  const [testingId, setTestingId] = useState<string | null>(null);
  const [probeNote, setProbeNote] = useState<{ tone: "ok" | "err"; text: string } | null>(null);
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
  const providers = snapshot?.providers ?? [];
  const origin = client.origin;

  const runProviderTest = async (id: string): Promise<void> => {
    setTestingId(id);
    setProbeNote(null);
    try {
      const res = await client.testProvider(id);
      const ms = typeof res.latency_ms === "number" ? Math.round(res.latency_ms) : null;
      setProbeNote({ tone: "ok", text: t.testOk(id, ms) });
      refresh();
    } catch (err) {
      setProbeNote({ tone: "err", text: t.testFail(id, errorMessage(err)) });
    } finally {
      setTestingId(null);
    }
  };

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

  const multiProviderSnippet = `# Per-session upstream provider selection (zero global config pollution)
lg cx -p openai
lg cc -p anthropic
lg oc -p gemini --opt passthrough

# Manage provider registry & credentials
lg provider list
lg provider add openai --driver openai --api-key-env OPENAI_API_KEY
lg provider test openai`;

  return (
    <>
      {providers.length > 0 && (
        <Card title={t.providers} sub={t.providersSub}>
          {probeNote && (
            <div className="mb-3">
              <Note tone={probeNote.tone} icon={probeNote.tone === "ok" ? <CircleCheck /> : <CircleAlert />}>
                {probeNote.text}
              </Note>
            </div>
          )}
          <div className="lg-table-wrap">
            <table className="lg-table">
              <thead>
                <tr>
                  <th>{t.colProvider}</th>
                  <th className="hide-sm">{t.colDriver}</th>
                  <th>{t.colModel}</th>
                  <th className="hide-sm">{t.colAuth}</th>
                  <th className="hide-sm">{t.colSpill}</th>
                  <th className="num">{t.colAction}</th>
                </tr>
              </thead>
              <tbody>
                {providers.map((p) => (
                  <tr key={p.id}>
                    <td className="whitespace-nowrap">
                      <div className="flex items-center gap-2">
                        <ProviderBadge id={p.id} className="font-medium text-fg" />
                        {p.active && <Pill tone="accent">{t.active}</Pill>}
                        {!p.enabled && <Pill tone="warn">{t.disabled}</Pill>}
                      </div>
                    </td>
                    <td className="lg-num hide-sm whitespace-nowrap text-[12px] text-fg-3">
                      {p.driver} · {p.wire_protocol}
                    </td>
                    <td className="lg-num whitespace-nowrap text-[12px]">{p.default_model}</td>
                    <td className="hide-sm whitespace-nowrap">
                      <div className="flex items-center gap-1.5">
                        <Pill tone={p.auth_configured ? "ok" : "warn"}>
                          {p.auth_configured ? t.authReady : t.authMissing}
                        </Pill>
                        <span className="lg-num text-[11.5px] text-fg-3">{p.auth_source}</span>
                      </div>
                    </td>
                    <td className="hide-sm whitespace-nowrap">
                      <Pill tone={p.spill_enabled ? "warn" : "ok"}>
                        {p.spill_enabled ? t.spillActive : t.spillNative}
                      </Pill>
                    </td>
                    <td className="num whitespace-nowrap">
                      <Button
                        size="sm"
                        variant="ghost"
                        disabled={testingId !== null}
                        onClick={() => void runProviderTest(p.id)}
                      >
                        <Play className="size-3.5" />
                        {testingId === p.id ? t.testing : t.testBtn}
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="mt-3">
            <CodeBlock code={multiProviderSnippet} language="sh" title={t.multiProviderCli} maxHeight={null} />
          </div>
        </Card>
      )}

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
