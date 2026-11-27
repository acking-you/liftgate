import * as React from "react";
import { useEffect, useState } from "react";
import {
  CircleAlert,
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
import { ConnectionModal, ProbeModal } from "@/components/modals";
import { ThemeToggle } from "@/components/ThemeToggle";
import { Button, Dot, Note } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { type ViewId, useHashRoute } from "@/hooks/useHashRoute";
import { CacheView } from "@/views/Cache";
import { CatalogView } from "@/views/Catalog";
import { ContextView } from "@/views/Context";
import { DiagnosticsView } from "@/views/Diagnostics";
import { ModelsView } from "@/views/Models";
import { OverviewView } from "@/views/Overview";
import { SessionsView } from "@/views/Sessions";
import { ToolCallsView } from "@/views/ToolCalls";

const SIDEBAR_KEY = "lg_sidebar_collapsed";

interface NavItem {
  id: ViewId;
  label: string;
  icon: React.ComponentType;
}

const NAV: ReadonlyArray<{ group: string; items: ReadonlyArray<NavItem> }> = [
  {
    group: "观测",
    items: [
      { id: "overview", label: "总览", icon: LayoutDashboard },
      { id: "tools", label: "工具调用", icon: Wrench },
      { id: "catalog", label: "工具目录", icon: Library },
      { id: "context", label: "完整上下文", icon: ScrollText },
      { id: "cache", label: "缓存", icon: Gauge },
    ],
  },
  {
    group: "基础设施",
    items: [
      { id: "sessions", label: "会话池", icon: Layers },
      { id: "models", label: "模型", icon: Sparkles },
      { id: "diagnostics", label: "诊断与接入", icon: Terminal },
    ],
  },
];

const TITLE: Record<ViewId, string> = Object.fromEntries(
  NAV.flatMap((g) => g.items.map((item) => [item.id, item.label])),
) as Record<ViewId, string>;

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
  const { client, snapshot, connectionState, lastError, lowCacheRecords } = useGateway();
  const { view, arg, navigate } = useHashRoute();
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
    document.title = `${TITLE[view]} · Liftgate`;
    setDrawer(false);
    window.scrollTo({ top: 0 });
  }, [view]);

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
  const counts: Partial<Record<ViewId, { text: string; tone?: "live" | "err" }>> = {
    overview: activeCount > 0 ? { text: String(activeCount), tone: "live" } : undefined,
    tools: snapshot?.tool_call_telemetry
      ? { text: String(snapshot.tool_call_telemetry.stored_records) }
      : undefined,
    context: snapshot?.context_capture?.enabled ? { text: "捕获中", tone: "live" } : undefined,
    cache: {
      text: String(snapshot?.cache_telemetry?.stored_low_cache_count ?? lowCacheRecords.length),
    },
    sessions: { text: String(snapshot?.session_pool.sessions.length ?? 0) },
    models: { text: String(snapshot?.available_models.length ?? 0) },
    diagnostics: stderrLines > 0 ? { text: String(stderrLines), tone: "err" } : undefined,
  };

  // One status, worst news first: no stream, then a dead ACP process, then live.
  const status: { tone: "ok" | "warn" | "err"; label: string; title: string } = client.demo
    ? { tone: "ok", label: "演示数据", title: "本地合成的演示数据" }
    : connectionState !== "live"
      ? {
          tone: lastError ? "err" : "warn",
          label: connectionState === "connecting" ? "连接中" : "重连中",
          title: "正在连接遥测事件流",
        }
      : acp && !acp.connected
        ? { tone: "err", label: "ACP 断开", title: "agy acp 主进程已断开" }
        : { tone: "ok", label: "实时", title: "遥测事件流已连接" };

  return (
    <div className="lg-shell" data-collapsed={collapsed} data-drawer={drawer}>
      <div className="lg-scrim" aria-hidden="true" onClick={() => setDrawer(false)} />

      <aside className="lg-side" aria-label="主导航">
        <div className="lg-brand">
          <BrandMark />
          <div className="lg-brand-text min-w-0">
            <div className="text-[14px] font-semibold leading-tight text-fg">Liftgate</div>
            <div className="lg-num text-[11px] leading-tight text-fg-4">
              {snapshot ? `v${snapshot.gateway_version}` : "ACP 网关"}
            </div>
          </div>
        </div>

        <nav className="lg-nav">
          {NAV.map((group) => (
            <div className="lg-nav-group" key={group.group}>
              <div className="lg-nav-label">{group.group}</div>
              {group.items.map((item) => {
                const Icon = item.icon;
                const count = counts[item.id];
                return (
                  <button
                    key={item.id}
                    type="button"
                    className="lg-nav-item"
                    aria-current={view === item.id ? "page" : undefined}
                    title={collapsed ? item.label : undefined}
                    onClick={() => navigate(item.id)}
                  >
                    <Icon />
                    <span className="lg-nav-text">{item.label}</span>
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
            aria-label="打开导航"
            onClick={() => setDrawer(true)}
          >
            <Menu />
          </Button>
          <Button
            variant="ghost"
            icon
            className="max-[900px]:hidden"
            aria-label={collapsed ? "展开侧栏" : "收起侧栏"}
            title={collapsed ? "展开侧栏" : "收起侧栏"}
            onClick={toggleCollapsed}
          >
            {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
          </Button>
          <h1 className="m-0 truncate text-[15px] font-semibold text-fg">{TITLE[view]}</h1>

          <div className="ml-auto flex items-center gap-1.5">
            <span
              className="mr-1 flex items-center gap-1.5 whitespace-nowrap text-[12px] text-fg-3"
              title={status.title}
            >
              <Dot tone={status.tone} live={status.tone !== "ok"} />
              {status.label}
            </span>
            <Button aria-label="拨测" onClick={() => openProbe()}>
              <Play />
              <span className="max-sm:hidden">拨测</span>
            </Button>
            <Button aria-label="连接设置" onClick={() => setConnectionOpen(true)}>
              <Settings2 />
              <span className="max-sm:hidden">连接</span>
            </Button>
            <ThemeToggle />
          </div>
        </header>

        <main className="lg-main">
          {client.demo && (
            <Note tone="accent" icon={<FlaskConical />}>
              演示数据，未连接网关。去掉地址里的 <code>?demo</code> 即连接真实网关。
            </Note>
          )}
          {lastError && !client.demo && (
            <Note tone="err" icon={<CircleAlert />}>
              连接不到网关（<code>{lastError}</code>）。确认 <code>lg start</code>{" "}
              已在运行，或在右上角「连接」里检查地址与密钥。
            </Note>
          )}

          {view === "overview" && <OverviewView arg={arg} navigate={navigate} />}
          {view === "tools" && <ToolCallsView arg={arg} navigate={navigate} />}
          {view === "catalog" && <CatalogView arg={arg} navigate={navigate} />}
          {view === "context" && <ContextView />}
          {view === "cache" && <CacheView arg={arg} navigate={navigate} />}
          {view === "sessions" && <SessionsView arg={arg} navigate={navigate} />}
          {view === "models" && <ModelsView onProbe={openProbe} />}
          {view === "diagnostics" && <DiagnosticsView />}
        </main>
      </div>

      <ConnectionModal open={connectionOpen} onOpenChange={setConnectionOpen} />
      <ProbeModal open={probeOpen} onOpenChange={setProbeOpen} model={probeModel} onModelChange={setProbeModel} />
    </div>
  );
}
