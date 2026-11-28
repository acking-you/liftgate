import * as React from "react";
import { useEffect, useState } from "react";
import {
  CircleAlert,
  CircleDollarSign,
  FlaskConical,
  Gauge,
  Layers,
  LayoutDashboard,
  Library,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  Play,
  ScrollText,
  Settings2,
  Sparkles,
  Terminal,
  Wrench,
} from "lucide-react";
import { LocaleToggle } from "@/components/LocaleToggle";
import { ConnectionModal, ProbeModal } from "@/components/modals";
import { ThemeToggle } from "@/components/ThemeToggle";
import { Button, Dot, Note } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { type ViewId, useHashRoute } from "@/hooks/useHashRoute";
import { useI18n } from "@/lib/i18n";
import { isFailure } from "@/lib/toolcalls";
import { CacheView } from "@/views/Cache";
import { CatalogView } from "@/views/Catalog";
import { ContextView } from "@/views/Context";
import { DiagnosticsView } from "@/views/Diagnostics";
import { ModelsView } from "@/views/Models";
import { OverviewView } from "@/views/Overview";
import { SessionsView } from "@/views/Sessions";
import { ToolCallsView } from "@/views/ToolCalls";
import { UsageView } from "@/views/Usage";

const SIDEBAR_KEY = "lg_sidebar_collapsed";

interface NavItem {
  id: ViewId;
  icon: React.ComponentType;
}

const NAV: ReadonlyArray<{ group: "observe" | "infra"; items: ReadonlyArray<NavItem> }> = [
  {
    group: "observe",
    items: [
      { id: "overview", icon: LayoutDashboard },
      { id: "usage", icon: CircleDollarSign },
      { id: "tools", icon: Wrench },
      { id: "catalog", icon: Library },
      { id: "context", icon: ScrollText },
      { id: "cache", icon: Gauge },
    ],
  },
  {
    group: "infra",
    items: [
      { id: "sessions", icon: Layers },
      { id: "models", icon: Sparkles },
      { id: "diagnostics", icon: Terminal },
    ],
  },
];

function BrandMark(): React.JSX.Element {
  return (
    <span className="lg-brand-mark" aria-hidden="true">
      <svg width="18" height="18" viewBox="0 0 32 32" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
        <path d="M8 25V13a8 8 0 0 1 16 0v12" />
        <path d="m12 19 4-4 4 4" />
      </svg>
    </span>
  );
}

export function App(): React.JSX.Element {
  const { client, snapshot, connectionState, lastError, lowCacheRecords, toolCalls } = useGateway();
  const { m } = useI18n();
  const { view, arg, navigate } = useHashRoute();
  const title = m.shell.views[view];
  const [collapsed, setCollapsed] = useState<boolean>(() => {
    try {
      return window.localStorage.getItem(SIDEBAR_KEY) === "1";
    } catch {
      return false;
    }
  });
  const [drawer, setDrawer] = useState(false);
  const [connectionOpen, setConnectionOpen] = useState(false);
  const [probeOpen, setProbeOpen] = useState(false);
  const [probeModel, setProbeModel] = useState("");

  const acp = snapshot?.acp_process;
  const defaultModel = acp?.default_model ?? "";
  useEffect(() => {
    if (probeModel === "" && defaultModel !== "") setProbeModel(defaultModel);
  }, [probeModel, defaultModel]);

  useEffect(() => {
    setDrawer(false);
    window.scrollTo({ top: 0 });
  }, [view]);

  useEffect(() => {
    document.title = `${title} · Liftgate`;
  }, [title]);

  const toggleCollapsed = (): void => {
    setCollapsed((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(SIDEBAR_KEY, next ? "1" : "0");
      } catch {
        // The preference simply does not persist when storage is unavailable.
      }
      return next;
    });
  };

  const openProbe = (modelId?: string): void => {
    if (modelId) setProbeModel(modelId);
    setProbeOpen(true);
  };

  const stderrLines = acp?.recent_stderr.length ?? 0;
  const activeCount = snapshot?.active_tasks.length ?? 0;
  const toolFailures = toolCalls.filter(isFailure).length;
  const counts: Partial<Record<ViewId, { text: string; tone?: "live" | "err"; title?: string }>> = {
    overview: activeCount > 0 ? { text: String(activeCount), tone: "live" } : undefined,
    usage: snapshot?.usage?.calls ? { text: String(snapshot.usage.calls) } : undefined,
    // Failures outrank volume: a red count is the cue to open the call stream.
    tools: !snapshot?.tool_call_telemetry
      ? undefined
      : toolFailures > 0
        ? { text: String(toolFailures), tone: "err", title: m.shell.toolFailuresBadge(toolFailures) }
        : { text: String(snapshot.tool_call_telemetry.stored_records) },
    context: snapshot?.context_capture?.enabled ? { text: m.shell.capturing, tone: "live" } : undefined,
    cache: {
      text: String(snapshot?.cache_telemetry?.stored_low_cache_count ?? lowCacheRecords.length),
    },
    sessions: { text: String(snapshot?.session_pool.sessions.length ?? 0) },
    models: { text: String(snapshot?.available_models.length ?? 0) },
    diagnostics: stderrLines > 0 ? { text: String(stderrLines), tone: "err" } : undefined,
  };

  // One status, worst news first: no stream, then a dead ACP process, then live.
  const st = m.shell.status;
  const status: { tone: "ok" | "warn" | "err"; label: string; title: string } = client.demo
    ? { tone: "ok", label: st.demo, title: st.demoTitle }
    : connectionState !== "live"
      ? {
          tone: lastError ? "err" : "warn",
          label: connectionState === "connecting" ? st.connecting : st.reconnecting,
          title: st.connectingTitle,
        }
      : acp && !acp.connected
        ? { tone: "err", label: st.acpDown, title: st.acpDownTitle }
        : { tone: "ok", label: st.live, title: st.liveTitle };

  return (
    <div className="lg-shell" data-collapsed={collapsed} data-drawer={drawer}>
      <div className="lg-scrim" aria-hidden="true" onClick={() => setDrawer(false)} />

      <aside className="lg-side" aria-label={m.shell.navAria}>
        <div className="lg-brand">
          <BrandMark />
          <div className="lg-brand-text min-w-0">
            <div className="text-[14px] font-semibold leading-tight text-fg">Liftgate</div>
            <div className="lg-num text-[11px] leading-tight text-fg-4">
              {snapshot ? `v${snapshot.gateway_version}` : m.shell.brandSub}
            </div>
          </div>
        </div>

        <nav className="lg-nav">
          {NAV.map((group) => (
            <div className="lg-nav-group" key={group.group}>
              <div className="lg-nav-label">{m.shell.groups[group.group]}</div>
              {group.items.map((item) => {
                const Icon = item.icon;
                const count = counts[item.id];
                const label = m.shell.views[item.id];
                return (
                  <button
                    key={item.id}
                    type="button"
                    className="lg-nav-item"
                    aria-current={view === item.id ? "page" : undefined}
                    title={collapsed ? label : count?.title}
                    onClick={() => navigate(item.id)}
                  >
                    <Icon />
                    <span className="lg-nav-text">{label}</span>
                    {count && (
                      <span className="lg-nav-count" data-tone={count.tone}>
                        {count.text}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          ))}
        </nav>

      </aside>

      <div className="min-w-0">
        <header className="lg-top">
          <Button
            variant="ghost"
            icon
            className="min-[901px]:hidden"
            aria-label={m.shell.openNav}
            onClick={() => setDrawer(true)}
          >
            <Menu />
          </Button>
          <Button
            variant="ghost"
            icon
            className="max-[900px]:hidden"
            aria-label={collapsed ? m.shell.expandSidebar : m.shell.collapseSidebar}
            title={collapsed ? m.shell.expandSidebar : m.shell.collapseSidebar}
            onClick={toggleCollapsed}
          >
            {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
          </Button>
          <h1 className="m-0 truncate text-[15px] font-semibold text-fg">{title}</h1>

          <div className="ml-auto flex items-center gap-1.5">
            <span
              className="mr-1 flex items-center gap-1.5 whitespace-nowrap text-[12px] text-fg-3"
              title={status.title}
            >
              <Dot tone={status.tone} live={status.tone !== "ok"} />
              <span className="max-sm:hidden">{status.label}</span>
            </span>
            <Button aria-label={m.shell.probe} onClick={() => openProbe()}>
              <Play />
              <span className="max-sm:hidden">{m.shell.probe}</span>
            </Button>
            <Button aria-label={m.shell.connectionSettings} onClick={() => setConnectionOpen(true)}>
              <Settings2 />
              <span className="max-sm:hidden">{m.shell.connection}</span>
            </Button>
            <LocaleToggle />
            <ThemeToggle />
          </div>
        </header>

        <main className="lg-main">
          {client.demo && (
            <Note tone="accent" icon={<FlaskConical />}>
              {m.shell.demoNote(<code>?demo</code>)}
            </Note>
          )}
          {lastError && !client.demo && (
            <Note tone="err" icon={<CircleAlert />}>
              {m.shell.unreachable(<code>{lastError}</code>, <code>lg start</code>)}
            </Note>
          )}

          {view === "overview" && <OverviewView arg={arg} navigate={navigate} />}
          {view === "usage" && <UsageView arg={arg} navigate={navigate} />}
          {view === "tools" && <ToolCallsView arg={arg} navigate={navigate} />}
          {view === "catalog" && <CatalogView arg={arg} navigate={navigate} />}
          {view === "context" && <ContextView arg={arg} navigate={navigate} />}
          {view === "cache" && <CacheView arg={arg} navigate={navigate} />}
          {view === "sessions" && <SessionsView arg={arg} navigate={navigate} />}
          {view === "models" && <ModelsView onProbe={openProbe} onEditPrices={() => navigate("usage")} />}
          {view === "diagnostics" && <DiagnosticsView />}
        </main>
      </div>

      <ConnectionModal open={connectionOpen} onOpenChange={setConnectionOpen} />
      <ProbeModal open={probeOpen} onOpenChange={setProbeOpen} model={probeModel} onModelChange={setProbeModel} />
    </div>
  );
}
