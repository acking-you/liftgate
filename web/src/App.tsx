import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertCircle,
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Clock,
  Cpu,
  Database,
  ExternalLink,
  Gauge,
  Globe,
  Layers,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  Play,
  RefreshCw,
  Search,
  Settings2,
  Sparkles,
  Terminal,
  Trash2,
  Wrench,
  X,
} from "lucide-react";
import { CacheTrendChart } from "@/components/CacheTrendChart";
import { CopyButton } from "@/components/CopyButton";
import { ThemeToggle } from "@/components/ThemeToggle";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useGatewayTelemetry } from "@/hooks/useGatewayTelemetry";
import type {
  LowCacheHitReason,
  LowCacheHitRecord,
  TaskPhase,
  TaskProtocol,
  TaskSnapshot,
} from "@/types/telemetry";

type WorkbenchTab = "overview" | "cache" | "sessions" | "models" | "logs";

const LOW_CACHE_PAGE_SIZE = 25;
const SIDEBAR_COLLAPSED_KEY = "sf_liftgate_sidebar_collapsed";

function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  const secs = ms / 1000;
  if (secs < 60) return `${secs.toFixed(1)} s`;
  const mins = Math.floor(secs / 60);
  const rem = Math.round(secs % 60);
  return `${mins}m ${rem}s`;
}

function formatUptime(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatTimestamp(ms: number): string {
  if (ms <= 0) return "-";
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

function formatDateTime(ms: number): string {
  if (ms <= 0) return "-";
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function formatNumber(n: number): string {
  return n.toLocaleString("en-US");
}

function formatCompactTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 10_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toLocaleString("en-US");
}

function formatBpsPct(bps: number | null | undefined, digits = 1): string {
  if (typeof bps !== "number" || !Number.isFinite(bps)) return "—";
  return `${(bps / 100).toFixed(digits)}%`;
}

function computeTaskHitBps(task: TaskSnapshot): number | null {
  if (typeof task.cache_hit_rate_bps === "number") {
    return task.cache_hit_rate_bps;
  }
  const u = task.usage;
  if (!u) return null;
  const prompt = u.inputTokens + u.cachedReadTokens + u.cachedWriteTokens;
  if (prompt <= 0) return null;
  return Math.round((Math.min(u.cachedReadTokens, prompt) * 10000) / prompt);
}

function renderPhasePill(phase: TaskPhase): React.JSX.Element {
  switch (phase) {
    case "initializing_session":
      return (
        <span className="sf-pill sf-pill--info sf-pill--live">
          <Clock className="h-3 w-3" /> 初始化会话
        </span>
      );
    case "thinking":
      return (
        <span className="sf-pill sf-pill--warn sf-pill--live">
          <Sparkles className="h-3 w-3" /> 深度思考
        </span>
      );
    case "searching_web":
      return (
        <span className="sf-pill sf-pill--info sf-pill--live">
          <Globe className="h-3 w-3" /> 联网搜索
        </span>
      );
    case "fetching_web":
      return (
        <span className="sf-pill sf-pill--info sf-pill--live">
          <Globe className="h-3 w-3" /> 网页抓取
        </span>
      );
    case "streaming_text":
      return (
        <span className="sf-pill sf-pill--accent sf-pill--live">
          <Activity className="h-3 w-3" /> 流式输出
        </span>
      );
    case "calling_client_tool":
      return (
        <span className="sf-pill sf-pill--warn">
          <Wrench className="h-3 w-3" /> 调用工具
        </span>
      );
    case "completed":
      return (
        <span className="sf-pill sf-pill--ok">
          <CheckCircle2 className="h-3 w-3" /> 已完成
        </span>
      );
    case "failed":
      return (
        <span className="sf-pill sf-pill--err">
          <AlertCircle className="h-3 w-3" /> 失败
        </span>
      );
    case "cancelled":
      return <span className="sf-pill">已取消</span>;
  }
}

function renderReasonPill(reason: LowCacheHitReason): React.JSX.Element {
  switch (reason) {
    case "warm_cache_miss":
      return <span className="sf-pill sf-pill--err">热会话掉缓存</span>;
    case "warm_partial_hit":
      return <span className="sf-pill sf-pill--warn">热会话部分命中</span>;
    case "cold_session_start":
      return <span className="sf-pill sf-pill--info">新建冷会话</span>;
    case "unknown":
      return <span className="sf-pill">未分类</span>;
  }
}

function explainLowCacheReason(record: LowCacheHitRecord): string {
  switch (record.reason) {
    case "warm_cache_miss":
      return "该轮复用了会话池中的 ACP 热会话，但上游返回 cachedReadTokens = 0。常见原因为会话空闲超过 5 分钟导致上游 KV 缓存自然过期，或在会话中途切换了模型。";
    case "warm_partial_hit":
      return "该轮复用了会话池热会话并命中了部分前缀缓存，但由于本轮新增了较大体积的工具输出或上下文增量，整体命中率低于设定阈值。";
    case "cold_session_start":
      return "该轮为全新对话首轮（或客户端前缀哈希未命中现有空闲会话），新建了独立的 ACP 子进程，因此无历史会话缓存可复用。";
    case "unknown":
      return "该轮 Prompt 缓存命中率低于配置阈值，已被自动写入本地持久化审计队列。";
  }
}

function classifyModelFamily(modelId: string): "gemini" | "claude" | "other" {
  const lower = modelId.toLowerCase();
  if (lower.includes("gemini")) return "gemini";
  if (lower.includes("claude")) return "claude";
  return "other";
}

export function App(): React.JSX.Element {
  const {
    snapshot,
    lowCacheRecords,
    lowCacheLoading,
    connectionState,
    lastError,
    baseUrl,
    apiKey,
    updateConnectionConfig,
    refreshSnapshot,
    refreshLowCacheHits,
    closeIdleSession,
    toggleVisionOffload,
    updateCacheConfig,
    clearLowCacheHits,
  } = useGatewayTelemetry();

  const [activeTab, setActiveTab] = useState<WorkbenchTab>("overview");
  const [sidebarCollapsed, setSidebarCollapsed] = useState<boolean>(() => {
    try {
      return window.localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === "1";
    } catch {
      return false;
    }
  });
  const [drawerOpen, setDrawerOpen] = useState(false);

  // Overview & Task filters
  const [protocolFilter, setProtocolFilter] = useState<"all" | TaskProtocol>("all");
  const [selectedTask, setSelectedTask] = useState<TaskSnapshot | null>(null);

  // Cache analytics & persistence config state
  const [reasonFilter, setReasonFilter] = useState<"all" | LowCacheHitReason>("all");
  const [cacheQuery, setCacheQuery] = useState("");
  const [cachePage, setCachePage] = useState(1);
  const [selectedLowRecord, setSelectedLowRecord] =
    useState<LowCacheHitRecord | null>(null);
  const [draftMaxRecords, setDraftMaxRecords] = useState<string>("1000");
  const [draftThresholdPct, setDraftThresholdPct] = useState<string>("50");
  const [cacheConfigDirty, setCacheConfigDirty] = useState(false);
  const [cacheConfigSaving, setCacheConfigSaving] = useState(false);
  const [cacheConfigBanner, setCacheConfigBanner] = useState<{
    tone: "ok" | "err";
    text: string;
  } | null>(null);
  const [confirmClearMode, setConfirmClearMode] = useState<
    null | "records" | "all"
  >(null);

  // Models filter
  const [modelFamily, setModelFamily] = useState<
    "all" | "gemini" | "claude" | "other"
  >("all");
  const [modelQuery, setModelQuery] = useState("");

  // Connection config modal state
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [draftBaseUrl, setDraftBaseUrl] = useState(baseUrl);
  const [draftApiKey, setDraftApiKey] = useState(apiKey);

  // Live probe modal state
  const [probeOpen, setProbeOpen] = useState(false);
  const [probeProtocol, setProbeProtocol] =
    useState<TaskProtocol>("openai_responses");
  const [probeModel, setProbeModel] = useState("gemini-4-hydrogen-low");
  const [probePrompt, setProbePrompt] = useState(
    "请用一句话介绍 Rust 2024 Edition 的优势。",
  );
  const [probeRunning, setProbeRunning] = useState(false);
  const [probeResult, setProbeResult] = useState<string | null>(null);

  const acp = snapshot?.acp_process;
  const pool = snapshot?.session_pool;
  const metrics = snapshot?.metrics;
  const cacheSummary = snapshot?.cache_telemetry;
  const activeTasks = snapshot?.active_tasks ?? [];
  const recentTasks = snapshot?.recent_tasks ?? [];
  const models = snapshot?.available_models ?? [];

  // Keep config inputs synchronized with live server values unless user is editing
  useEffect(() => {
    if (!cacheSummary || cacheConfigDirty) return;
    setDraftMaxRecords(String(cacheSummary.max_low_cache_records));
    setDraftThresholdPct(
      String(Math.round(cacheSummary.low_cache_threshold_bps / 100)),
    );
  }, [
    cacheSummary?.max_low_cache_records,
    cacheSummary?.low_cache_threshold_bps,
    cacheConfigDirty,
    cacheSummary,
  ]);

  const effectiveThresholdBps = cacheSummary?.low_cache_threshold_bps ?? 5000;
  const effectiveMaxLowRecords = cacheSummary?.max_low_cache_records ?? 1000;

  // Overall cache hit rate (persisted store first, fallback to runtime metrics)
  const overallCacheStats = useMemo(() => {
    if (cacheSummary && cacheSummary.total_prompt_tokens > 0) {
      return {
        promptTokens: cacheSummary.total_prompt_tokens,
        cachedRead: cacheSummary.total_cached_read_tokens,
        cachedWrite: cacheSummary.total_cached_write_tokens,
        uncachedInput: cacheSummary.total_uncached_input_tokens,
        hitBps: cacheSummary.total_cache_hit_rate_bps,
        warmHitBps: cacheSummary.warm_cache_hit_rate_bps,
        evaluatedTurns: cacheSummary.evaluated_turns,
        warmTurns: cacheSummary.warm_evaluated_turns,
      };
    }
    const uncached = metrics?.total_input_tokens ?? 0;
    const cachedRead = metrics?.total_cached_tokens ?? 0;
    const cachedWrite = metrics?.total_cached_write_tokens ?? 0;
    const promptTokens = uncached + cachedRead + cachedWrite;
    const hitBps =
      metrics?.cache_hit_rate_bps ??
      (promptTokens > 0
        ? Math.round((Math.min(cachedRead, promptTokens) * 10000) / promptTokens)
        : null);
    return {
      promptTokens,
      cachedRead,
      cachedWrite,
      uncachedInput: uncached,
      hitBps,
      warmHitBps: null,
      evaluatedTurns: metrics?.completed_requests ?? 0,
      warmTurns: 0,
    };
  }, [cacheSummary, metrics]);

  const sessionHitRate = useMemo(() => {
    if (!pool) return 0;
    const total = pool.total_sessions_created + pool.total_sessions_reused;
    if (total === 0) return 0;
    return Math.round((pool.total_sessions_reused / total) * 100);
  }, [pool]);

  const idlePoolPercent = useMemo(() => {
    if (!pool || pool.max_idle_sessions === 0) return 0;
    return Math.min(
      100,
      Math.round((pool.idle_cached_sessions / pool.max_idle_sessions) * 100),
    );
  }, [pool]);

  const filteredRecentTasks = useMemo(() => {
    if (protocolFilter === "all") return recentTasks;
    return recentTasks.filter((t) => t.protocol === protocolFilter);
  }, [recentTasks, protocolFilter]);

  const reasonCounts = useMemo(() => {
    let warmMiss = 0;
    let warmPartial = 0;
    let coldStart = 0;
    for (const r of lowCacheRecords) {
      if (r.reason === "warm_cache_miss") warmMiss += 1;
      else if (r.reason === "warm_partial_hit") warmPartial += 1;
      else if (r.reason === "cold_session_start") coldStart += 1;
    }
    return { warmMiss, warmPartial, coldStart };
  }, [lowCacheRecords]);

  const filteredLowCacheRecords = useMemo(() => {
    const q = cacheQuery.trim().toLowerCase();
    return lowCacheRecords.filter((rec) => {
      if (reasonFilter !== "all" && rec.reason !== reasonFilter) {
        return false;
      }
      if (!q) return true;
      return (
        rec.task_id.toLowerCase().includes(q) ||
        rec.requested_model.toLowerCase().includes(q) ||
        rec.resolved_model.toLowerCase().includes(q) ||
        (rec.session_id?.toLowerCase().includes(q) ?? false) ||
        rec.prompt_preview.toLowerCase().includes(q)
      );
    });
  }, [lowCacheRecords, reasonFilter, cacheQuery]);

  const totalLowCachePages = Math.max(
    1,
    Math.ceil(filteredLowCacheRecords.length / LOW_CACHE_PAGE_SIZE),
  );
  const clampedCachePage = Math.min(cachePage, totalLowCachePages);
  const pagedLowCacheRecords = useMemo(() => {
    const start = (clampedCachePage - 1) * LOW_CACHE_PAGE_SIZE;
    return filteredLowCacheRecords.slice(start, start + LOW_CACHE_PAGE_SIZE);
  }, [filteredLowCacheRecords, clampedCachePage]);

  const filteredModels = useMemo(() => {
    const q = modelQuery.trim().toLowerCase();
    return models.filter((m) => {
      if (modelFamily !== "all" && classifyModelFamily(m.modelId) !== modelFamily) {
        return false;
      }
      if (!q) return true;
      return (
        m.modelId.toLowerCase().includes(q) || m.name.toLowerCase().includes(q)
      );
    });
  }, [models, modelFamily, modelQuery]);

  const effectiveGatewayOrigin = useMemo(() => {
    if (baseUrl.trim().length > 0) return baseUrl.trim().replace(/\/+$/, "");
    if (typeof window !== "undefined" && window.location.port !== "5173") {
      return window.location.origin;
    }
    return "http://127.0.0.1:8787";
  }, [baseUrl]);

  const codexConfigSnippet = useMemo(
    () => `# 推荐一键启动（自动从 /v1/codex-models 同步全量模型目录，不污染全局配置）
lg cx   # 或启动桌面端: lg cxa

# 或手动写入 ~/.codex/config.toml
model = "${acp?.default_model ?? "gemini-4-carbon-high"}"
model_provider = "agy"
model_catalog_json = "~/.codex/agy-models.json"
web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "${effectiveGatewayOrigin}/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"`,
    [acp?.default_model, effectiveGatewayOrigin],
  );

  const claudeEnvSnippet = useMemo(
    () => `# 推荐一键启动（自动注入 500k 逐模型压缩窗口，退出自动还原）
lg cc   # 或启动桌面端: lg cd

# Linux / macOS 手动环境变量
export ANTHROPIC_BASE_URL="${effectiveGatewayOrigin}"
export ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
export ANTHROPIC_MODEL="${acp?.default_model ?? "gemini-4-carbon-high"}"
claude

# Windows PowerShell 手动环境变量
$env:ANTHROPIC_BASE_URL="${effectiveGatewayOrigin}"
$env:ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
$env:ANTHROPIC_MODEL="${acp?.default_model ?? "gemini-4-carbon-high"}"
claude`,
    [acp?.default_model, apiKey, effectiveGatewayOrigin],
  );

  const toggleSidebarCollapse = (): void => {
    setSidebarCollapsed((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(SIDEBAR_COLLAPSED_KEY, next ? "1" : "0");
      } catch {
        // Ignore storage write errors
      }
      return next;
    });
  };

  const selectTab = (tab: WorkbenchTab): void => {
    setActiveTab(tab);
    setDrawerOpen(false);
  };

  const handleSaveCacheConfig = async (ev: React.FormEvent): Promise<void> => {
    ev.preventDefault();
    const parsedMax = Number.parseInt(draftMaxRecords.trim(), 10);
    const parsedPct = Number.parseInt(draftThresholdPct.trim(), 10);
    if (!Number.isFinite(parsedMax) || parsedMax < 0 || parsedMax > 50_000) {
      setCacheConfigBanner({
        tone: "err",
        text: "最大保留记录条数需在 0 到 50,000 之间（默认 1000，设为 0 则关闭记录）。",
      });
      return;
    }
    if (!Number.isFinite(parsedPct) || parsedPct < 0 || parsedPct > 100) {
      setCacheConfigBanner({
        tone: "err",
        text: "低缓存命中率判定阈值需在 0% 到 100% 之间（默认 50%）。",
      });
      return;
    }

    setCacheConfigSaving(true);
    setCacheConfigBanner(null);
    const res = await updateCacheConfig(parsedMax, parsedPct);
    setCacheConfigSaving(false);

    if (res.ok) {
      setCacheConfigDirty(false);
      setCachePage(1);
      setCacheConfigBanner({
        tone: "ok",
        text: `已生效并持久化：最多保留 ${formatNumber(res.summary.max_low_cache_records)} 条低命中记录，判定阈值 < ${Math.round(res.summary.low_cache_threshold_bps / 100)}%。`,
      });
    } else {
      setCacheConfigBanner({
        tone: "err",
        text: `保存失败：${res.error}`,
      });
    }
  };

  const handleConfirmClear = async (resetTotals: boolean): Promise<void> => {
    setConfirmClearMode(null);
    const summary = await clearLowCacheHits(resetTotals);
    if (summary) {
      setCachePage(1);
      setCacheConfigBanner({
        tone: "ok",
        text: resetTotals
          ? "已清空全部持久化低命中记录并重置累计缓存统计计数。"
          : "已清空全部持久化低命中记录（累计缓存命中率统计保留）。",
      });
    }
  };

  const runLiveProbe = async (): Promise<void> => {
    setProbeRunning(true);
    setProbeResult(null);
    try {
      const headers: Record<string, string> = {
        "Content-Type": "application/json",
      };
      if (apiKey.trim().length > 0) {
        headers["Authorization"] = `Bearer ${apiKey.trim()}`;
      }
      const prefix = baseUrl.trim().replace(/\/+$/, "");
      if (probeProtocol === "openai_responses") {
        const resp = await fetch(`${prefix}/v1/responses`, {
          method: "POST",
          headers,
          body: JSON.stringify({
            model: probeModel,
            stream: false,
            input: probePrompt,
          }),
        });
        const data = (await resp.json()) as {
          id?: string;
          output?: Array<{
            type?: string;
            content?: Array<{ text?: string }>;
          }>;
          error?: { message?: string };
        };
        if (!resp.ok) {
          throw new Error(data.error?.message ?? `HTTP ${resp.status}`);
        }
        const msg =
          data.output?.find((o) => o.type === "message")?.content?.[0]?.text ??
          JSON.stringify(data);
        setProbeResult(`[${data.id ?? "ok"}] ${msg}`);
      } else {
        const resp = await fetch(`${prefix}/v1/messages`, {
          method: "POST",
          headers,
          body: JSON.stringify({
            model: probeModel,
            max_tokens: 1024,
            stream: false,
            messages: [{ role: "user", content: probePrompt }],
          }),
        });
        const data = (await resp.json()) as {
          id?: string;
          content?: Array<{ type?: string; text?: string }>;
          error?: { message?: string };
        };
        if (!resp.ok) {
          throw new Error(data.error?.message ?? `HTTP ${resp.status}`);
        }
        const txt =
          data.content?.find((c) => c.type === "text")?.text ??
          JSON.stringify(data);
        setProbeResult(`[${data.id ?? "ok"}] ${txt}`);
      }
    } catch (err) {
      setProbeResult(
        `请求错误: ${err instanceof Error ? err.message : String(err)}`,
      );
    } finally {
      setProbeRunning(false);
    }
  };

  const tabMeta: Record<WorkbenchTab, { group: string; label: string }> = {
    overview: { group: "观测与审计", label: "运行总览与实时流" },
    cache: { group: "观测与审计", label: "缓存命中与低命中记录" },
    sessions: { group: "基础设施", label: "ACP 会话池与监管" },
    models: { group: "基础设施", label: "模型目录与实测" },
    logs: { group: "基础设施", label: "诊断日志与接入" },
  };

  const storedLowCount =
    cacheSummary?.stored_low_cache_count ?? lowCacheRecords.length;

  return (
    <div
      className={`sf-shell${sidebarCollapsed ? " is-collapsed" : ""}${drawerOpen ? " is-drawer-open" : ""}`}
    >
      <div
        className="sf-scrim"
        aria-hidden="true"
        onClick={() => setDrawerOpen(false)}
      />

      {/* Editorial Sidebar */}
      <aside className="sf-sidebar" aria-label="工作台主导航">
        <div className="sf-brand">
          <div className="sf-brand__identity">
            <span className="sf-brand__mark" aria-hidden="true" />
            <div className="sf-brand__text">
              <span className="sf-brand__name">Liftgate</span>
              <span className="sf-brand__sub">
                v{snapshot?.gateway_version ?? "0.2.5"} · ACP GATEWAY
              </span>
            </div>
          </div>

          <button
            id="btn-sidebar-collapse"
            type="button"
            className="sf-btn sf-btn--ghost sf-btn--icon sf-desktop-only"
            title={sidebarCollapsed ? "展开侧栏" : "收起侧栏"}
            aria-label={sidebarCollapsed ? "展开侧栏" : "收起侧栏"}
            onClick={toggleSidebarCollapse}
          >
            {sidebarCollapsed ? (
              <PanelLeftOpen className="h-4 w-4" />
            ) : (
              <PanelLeftClose className="h-4 w-4" />
            )}
          </button>

          <button
            id="btn-drawer-close"
            type="button"
            className="sf-btn sf-btn--ghost sf-btn--icon sf-mobile-only"
            aria-label="关闭导航抽屉"
            onClick={() => setDrawerOpen(false)}
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <nav className="sf-nav">
          <div className="sf-nav-group">
            <div className="sf-nav-label">观测与审计</div>
            <button
              id="nav-overview"
              type="button"
              className={`sf-nav-item${activeTab === "overview" ? " is-active" : ""}`}
              onClick={() => selectTab("overview")}
            >
              <Activity className="sf-nav-icon" />
              <span className="sf-nav-text">运行总览与实时流</span>
              {activeTasks.length > 0 && (
                <span className="sf-nav-badge sf-nav-badge--accent">
                  {activeTasks.length}
                </span>
              )}
            </button>

            <button
              id="nav-cache"
              type="button"
              className={`sf-nav-item${activeTab === "cache" ? " is-active" : ""}`}
              onClick={() => selectTab("cache")}
            >
              <Gauge className="sf-nav-icon" />
              <span className="sf-nav-text">缓存命中与低命中记录</span>
              <span
                className={`sf-nav-badge${reasonCounts.warmMiss > 0 ? " sf-nav-badge--warn" : ""}`}
              >
                {storedLowCount}
              </span>
            </button>
          </div>

          <div className="sf-nav-group">
            <div className="sf-nav-label">基础设施</div>
            <button
              id="nav-sessions"
              type="button"
              className={`sf-nav-item${activeTab === "sessions" ? " is-active" : ""}`}
              onClick={() => selectTab("sessions")}
            >
              <Layers className="sf-nav-icon" />
              <span className="sf-nav-text">ACP 会话池与监管</span>
              <span className="sf-nav-badge">{pool?.sessions.length ?? 0}</span>
            </button>

            <button
              id="nav-models"
              type="button"
              className={`sf-nav-item${activeTab === "models" ? " is-active" : ""}`}
              onClick={() => selectTab("models")}
            >
              <Sparkles className="sf-nav-icon" />
              <span className="sf-nav-text">模型目录与实测</span>
              <span className="sf-nav-badge">{models.length}</span>
            </button>

            <button
              id="nav-logs"
              type="button"
              className={`sf-nav-item${activeTab === "logs" ? " is-active" : ""}`}
              onClick={() => selectTab("logs")}
            >
              <Terminal className="sf-nav-icon" />
              <span className="sf-nav-text">诊断日志与接入</span>
              {(acp?.recent_stderr.length ?? 0) > 0 && (
                <span className="sf-nav-badge sf-nav-badge--warn">
                  {acp?.recent_stderr.length}
                </span>
              )}
            </button>
          </div>
        </nav>

        <div className="sf-sidebar__foot">
          <div className="sf-sidebar__foot-detail space-y-1.5">
            <div className="flex items-center justify-between font-mono text-[11px]">
              <span>总缓存命中</span>
              <strong className="text-[var(--text)]">
                {formatBpsPct(overallCacheStats.hitBps)}
              </strong>
            </div>
            <div className="flex items-center justify-between font-mono text-[11px]">
              <span>默认模型</span>
              <span className="truncate max-w-[128px] text-[var(--text-2)]">
                {acp?.default_model ?? "-"}
              </span>
            </div>
            <a
              href="https://github.com/acking-you/liftgate"
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 pt-1 text-[11.5px] text-[var(--muted)] hover:text-[var(--primary)]"
            >
              <span>GitHub · acking-you/liftgate</span>
              <ExternalLink className="h-3 w-3" />
            </a>
          </div>
        </div>
      </aside>

      {/* Main Content Frame */}
      <div className="sf-frame">
        {/* Sticky Acrylic Topbar */}
        <header className="sf-topbar">
          <button
            id="btn-drawer-open"
            type="button"
            className="sf-btn sf-btn--ghost sf-btn--icon sf-mobile-only"
            aria-label="打开导航菜单"
            onClick={() => setDrawerOpen(true)}
          >
            <Menu className="h-4 w-4" />
          </button>

          <div className="sf-crumbs">
            <span className="sf-crumbs__group sf-hide-xs">
              {tabMeta[activeTab].group}
            </span>
            <span className="sf-crumbs__sep sf-hide-xs">/</span>
            <span className="sf-crumbs__page">{tabMeta[activeTab].label}</span>
          </div>

          <div className="sf-topbar__right">
            <div
              className={`sf-health ${
                connectionState === "live"
                  ? "sf-health--ok"
                  : lastError
                    ? "sf-health--err"
                    : "sf-health--warn"
              }`}
              title={
                connectionState === "live"
                  ? "SSE 实时遥测流已建立"
                  : "正在连接或重连 SSE 遥测流"
              }
            >
              <span className="sf-health__dot" />
              <span className="sf-hide-xs">
                {connectionState === "live"
                  ? "SSE 实时连接"
                  : connectionState === "connecting"
                    ? "连接中…"
                    : "SSE 重连中"}
              </span>
            </div>

            {acp && (
              <div
                className={`sf-health sf-hide-xs ${
                  acp.connected ? "sf-health--ok" : "sf-health--err"
                }`}
              >
                <Cpu className="h-3.5 w-3.5" />
                <span className="font-mono text-[11.5px]">
                  {acp.connected
                    ? `ACP${acp.pid !== null ? ` · ${acp.pid}` : ""}`
                    : "ACP 断开"}
                </span>
              </div>
            )}

            <button
              id="btn-probe-open"
              type="button"
              className="sf-btn sf-btn--sm"
              onClick={() => setProbeOpen(true)}
            >
              <Play className="h-3.5 w-3.5 text-[var(--primary)]" />
              <span className="sf-hide-xs">在线拨测</span>
            </button>

            <button
              id="btn-settings-open"
              type="button"
              className="sf-btn sf-btn--sm"
              onClick={() => {
                setDraftBaseUrl(baseUrl);
                setDraftApiKey(apiKey);
                setSettingsOpen(true);
              }}
            >
              <Settings2 className="h-3.5 w-3.5" />
              <span className="sf-hide-xs">连接</span>
            </button>

            <button
              id="btn-refresh-snapshot"
              type="button"
              className="sf-btn sf-btn--ghost sf-btn--icon"
              title="立即刷新快照"
              aria-label="立即刷新快照"
              onClick={() => {
                void refreshSnapshot();
                void refreshLowCacheHits();
              }}
            >
              <RefreshCw className="h-3.5 w-3.5" />
            </button>

            <ThemeToggle id="btn-theme-toggle" />
          </div>
        </header>

        <main className="sf-main space-y-6">
          {lastError && (
            <div className="sf-banner sf-banner--err" role="alert">
              <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
              <div>
                无法连接网关遥测接口（<code>{lastError}</code>）。请确认{" "}
                <code className="rounded bg-[var(--surface)] px-1.5 py-0.5 text-xs">
                  lg start
                </code>{" "}
                或{" "}
                <code className="rounded bg-[var(--surface)] px-1.5 py-0.5 text-xs">
                  liftgate
                </code>{" "}
                已启动，或在右上角「连接」检查网关地址与密钥。
              </div>
            </div>
          )}

          {/* ===============================================================
             VIEW 1: OVERVIEW & REAL-TIME TASK STREAM
             =============================================================== */}
          {activeTab === "overview" && (
            <>
              <header className="sf-page-header">
                <div>
                  <p className="sf-eyebrow">OBSERVABILITY · REAL-TIME GATEWAY</p>
                  <h1 className="sf-title">运行总览与实时态势</h1>
                  <p className="sf-desc">
                    基于官方 <code>agy acp --process-per-session</code> 标准协议管道，实时追踪
                    OpenAI Responses 与 Anthropic Messages 双栈推理流、Prompt 缓存命中率及多轮会话池复用效率。
                  </p>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  {acp && (
                    <span className="sf-tag">
                      运行时长 {formatUptime(acp.uptime_secs)}
                    </span>
                  )}
                  <button
                    id="btn-overview-jump-cache"
                    type="button"
                    className="sf-btn sf-btn--sm"
                    onClick={() => setActiveTab("cache")}
                  >
                    <Gauge className="h-3.5 w-3.5 text-[var(--primary)]" />
                    查看低缓存命中审计 ({storedLowCount})
                  </button>
                </div>
              </header>

              {/* 5-Cell Hairline KPI Strip */}
              <section className="sf-kpis" aria-label="核心运行指标">
                <button
                  id="kpi-overall-cache-hit"
                  type="button"
                  className="sf-kpi sf-kpi--interactive"
                  onClick={() => setActiveTab("cache")}
                >
                  <div className="sf-kpi__label">
                    <span>总 Prompt 缓存命中率</span>
                    <Gauge className="h-3.5 w-3.5 text-[var(--primary)]" />
                  </div>
                  <div className="sf-kpi__value">
                    {formatBpsPct(overallCacheStats.hitBps, 1)}
                  </div>
                  <div className="space-y-1.5 w-full">
                    <div className="sf-progress">
                      <span
                        style={{
                          width: `${Math.min(100, Math.round((overallCacheStats.hitBps ?? 0) / 100))}%`,
                          background:
                            overallCacheStats.hitBps !== null &&
                            overallCacheStats.hitBps < effectiveThresholdBps
                              ? "var(--warn)"
                              : "var(--ok)",
                        }}
                      />
                    </div>
                    <div className="sf-kpi__foot flex justify-between gap-2">
                      <span>
                        热续接 {formatBpsPct(overallCacheStats.warmHitBps, 1)}
                      </span>
                      <span>
                        缓存↓ {formatCompactTokens(overallCacheStats.cachedRead)}
                      </span>
                    </div>
                  </div>
                </button>

                <button
                  id="kpi-low-cache-records"
                  type="button"
                  className="sf-kpi sf-kpi--interactive"
                  onClick={() => setActiveTab("cache")}
                >
                  <div className="sf-kpi__label">
                    <span>低命中率持久化记录</span>
                    <Database className="h-3.5 w-3.5 text-[var(--warn)]" />
                  </div>
                  <div className="sf-kpi__value">
                    {formatNumber(storedLowCount)}{" "}
                    <span className="font-mono text-sm text-[var(--faint)]">
                      / {formatNumber(effectiveMaxLowRecords)}
                    </span>
                  </div>
                  <div className="sf-kpi__foot flex justify-between gap-2">
                    <span>阈值 &lt;{Math.round(effectiveThresholdBps / 100)}%</span>
                    <span>
                      热掉缓存{" "}
                      <strong
                        className={
                          reasonCounts.warmMiss > 0
                            ? "text-[var(--err)]"
                            : "text-[var(--muted)]"
                        }
                      >
                        {reasonCounts.warmMiss}
                      </strong>
                    </span>
                  </div>
                </button>

                <button
                  id="kpi-session-pool"
                  type="button"
                  className="sf-kpi sf-kpi--interactive"
                  onClick={() => setActiveTab("sessions")}
                >
                  <div className="sf-kpi__label">
                    <span>多轮会话池复用率</span>
                    <Layers className="h-3.5 w-3.5 text-[var(--ok)]" />
                  </div>
                  <div className="sf-kpi__value">{sessionHitRate}%</div>
                  <div className="space-y-1.5 w-full">
                    <div className="sf-progress">
                      <span style={{ width: `${idlePoolPercent}%` }} />
                    </div>
                    <div className="sf-kpi__foot flex justify-between gap-2">
                      <span>
                        空闲 {pool?.idle_cached_sessions ?? 0}/
                        {pool?.max_idle_sessions ?? 0}
                      </span>
                      <span>
                        复用 {pool?.total_sessions_reused ?? 0} · 新建{" "}
                        {pool?.total_sessions_created ?? 0}
                      </span>
                    </div>
                  </div>
                </button>

                <div className="sf-kpi">
                  <div className="sf-kpi__label">
                    <span>执行中 / 累计请求</span>
                    <Activity className="h-3.5 w-3.5 text-[var(--info)]" />
                  </div>
                  <div className="sf-kpi__value">
                    {activeTasks.length}{" "}
                    <span className="font-mono text-sm text-[var(--faint)]">
                      / {formatNumber(metrics?.total_requests ?? 0)}
                    </span>
                  </div>
                  <div className="sf-kpi__foot flex justify-between gap-2">
                    <span className="text-[var(--ok)]">
                      成功 {formatNumber(metrics?.completed_requests ?? 0)}
                    </span>
                    <span
                      className={
                        (metrics?.failed_requests ?? 0) > 0
                          ? "text-[var(--err)]"
                          : ""
                      }
                    >
                      失败 {metrics?.failed_requests ?? 0}
                    </span>
                  </div>
                </div>

                <div className="sf-kpi">
                  <div className="sf-kpi__label">
                    <span>累计 Token 流转</span>
                    <Sparkles className="h-3.5 w-3.5 text-[var(--primary)]" />
                  </div>
                  <div className="sf-kpi__value">
                    {formatCompactTokens(
                      overallCacheStats.promptTokens +
                        (metrics?.total_output_tokens ?? 0),
                    )}
                  </div>
                  <div className="sf-kpi__foot flex justify-between gap-2">
                    <span>总输入 {formatCompactTokens(overallCacheStats.promptTokens)}</span>
                    <span>
                      输出 {formatCompactTokens(metrics?.total_output_tokens ?? 0)}
                    </span>
                  </div>
                </div>
              </section>

              {/* Cache Hit Rate Trend Chart Card */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      近期轮次 Prompt 缓存命中率曲线
                    </h2>
                    <p className="sf-card__sub">
                      展示最近已完成请求的逐轮缓存命中率与持久化告警线，悬停或点击圆点可检视该轮详情
                    </p>
                  </div>
                  <button
                    type="button"
                    className="sf-btn sf-btn--ghost sf-btn--sm"
                    onClick={() => setActiveTab("cache")}
                  >
                    调整阈值与持久化策略 →
                  </button>
                </div>
                <div className="sf-card__body">
                  <CacheTrendChart
                    recentTasks={recentTasks}
                    thresholdBps={effectiveThresholdBps}
                    onSelectTask={(task) => setSelectedTask(task)}
                  />
                </div>
              </section>

              {/* In-Flight Active Tasks */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      正在执行的推理流（In-Flight · {activeTasks.length}）
                    </h2>
                    <p className="sf-card__sub">
                      实时呈现活跃请求的思考进度、原生联网检索、客户端工具调用与流式输出预览
                    </p>
                  </div>
                  <span className="sf-tag">
                    活跃子进程 {pool?.active_leased_sessions ?? 0}
                  </span>
                </div>
                <div className="sf-card__body">
                  {activeTasks.length === 0 ? (
                    <div className="sf-empty py-7">
                      <p className="sf-empty__title">当前暂无执行中的请求</p>
                      <p className="sf-empty__hint">
                        通过 <code>lg cx</code>、<code>lg cc</code> 发起编码对话，或点击右上角「在线拨测」即可实时观测流式推理。
                      </p>
                    </div>
                  ) : (
                    <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
                      {activeTasks.map((task) => (
                        <div
                          key={task.task_id}
                          className="space-y-2.5 rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-4"
                        >
                          <div className="flex flex-wrap items-center justify-between gap-2">
                            <div className="flex flex-wrap items-center gap-1.5">
                              {renderPhasePill(task.phase)}
                              <span className="sf-tag">
                                {task.protocol === "openai_responses"
                                  ? "/v1/responses"
                                  : "/v1/messages"}
                              </span>
                            </div>
                            <span className="font-mono text-xs text-[var(--muted)] tabular-nums">
                              已耗时 {formatDurationMs(task.elapsed_ms)}
                              {task.ttft_ms != null
                                ? ` · TTFT ${formatDurationMs(task.ttft_ms)}`
                                : ""}
                            </span>
                          </div>

                          <div className="flex flex-wrap items-center gap-2 text-xs">
                            <code className="font-mono font-medium text-[var(--text)]">
                              {task.task_id}
                            </code>
                            <span className="font-mono text-[var(--muted)]">
                              {task.requested_model === task.resolved_model
                                ? task.resolved_model
                                : `${task.requested_model} → ${task.resolved_model}`}
                            </span>
                            {task.session_id && (
                              <span
                                className={`sf-pill ${task.session_reused ? "sf-pill--ok" : ""}`}
                              >
                                {task.session_reused ? "复用热会话" : "新建会话"} ·{" "}
                                {task.session_id.slice(0, 10)}
                              </span>
                            )}
                          </div>

                          {task.prompt_preview && (
                            <p className="line-clamp-2 rounded-[6px] border border-[var(--border)] bg-[var(--surface)] px-2.5 py-1.5 text-xs text-[var(--muted)]">
                              <strong className="text-[var(--text-2)]">输入: </strong>
                              {task.prompt_preview}
                            </p>
                          )}

                          {task.output_preview && (
                            <p className="line-clamp-2 rounded-[6px] border border-[var(--border)] bg-[var(--primary-soft)] px-2.5 py-1.5 text-xs text-[var(--text)]">
                              <strong className="text-[var(--primary)]">流式输出: </strong>
                              {task.output_preview}
                            </p>
                          )}

                          <div className="flex flex-wrap items-center gap-3 font-mono text-[11.5px] text-[var(--faint)]">
                            <span>思考 {task.thought_chars} chars</span>
                            <span>输出 {task.output_chars} chars</span>
                            {task.native_web_searches > 0 && (
                              <span className="text-[var(--info)]">
                                Web 搜索 ×{task.native_web_searches}
                              </span>
                            )}
                            {(task.native_web_fetches ?? 0) > 0 && (
                              <span className="text-[var(--ok)]">
                                网页抓取 ×{task.native_web_fetches}
                              </span>
                            )}
                            {task.client_tool_calls.length > 0 && (
                              <span className="text-[var(--warn)]">
                                工具: {task.client_tool_calls.join(", ")}
                              </span>
                            )}
                          </div>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </section>

              {/* Recent Completed Tasks Table */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      最近完成任务明细（内存保留最近 100 条）
                    </h2>
                    <p className="sf-card__sub">
                      点击任意记录行可查看完整输入输出摘要、工具调用链与单轮缓存命中明细
                    </p>
                  </div>
                  <div className="sf-seg" role="tablist" aria-label="协议筛选">
                    <button
                      type="button"
                      className={`sf-seg__btn${protocolFilter === "all" ? " is-active" : ""}`}
                      onClick={() => setProtocolFilter("all")}
                    >
                      全部 ({recentTasks.length})
                    </button>
                    <button
                      type="button"
                      className={`sf-seg__btn${protocolFilter === "openai_responses" ? " is-active" : ""}`}
                      onClick={() => setProtocolFilter("openai_responses")}
                    >
                      OpenAI Responses
                    </button>
                    <button
                      type="button"
                      className={`sf-seg__btn${protocolFilter === "anthropic_messages" ? " is-active" : ""}`}
                      onClick={() => setProtocolFilter("anthropic_messages")}
                    >
                      Anthropic Messages
                    </button>
                  </div>
                </div>
                <div className="sf-card__body sf-card__body--flush">
                  {filteredRecentTasks.length === 0 ? (
                    <div className="sf-empty">
                      <p className="sf-empty__title">暂无历史任务记录</p>
                      <p className="sf-empty__hint">
                        完成至少一次推理请求后，详细耗时、Token 构成与缓存命中率将展示在此处。
                      </p>
                    </div>
                  ) : (
                    <div className="sf-table-wrap">
                      <table className="sf-table">
                        <thead>
                          <tr>
                            <th>时间</th>
                            <th>任务 / 协议</th>
                            <th>模型路由</th>
                            <th>会话状态</th>
                            <th>状态与工具</th>
                            <th className="num">缓存命中率</th>
                            <th className="num">Prompt / Out</th>
                            <th className="num">耗时 / TTFT</th>
                          </tr>
                        </thead>
                        <tbody>
                          {filteredRecentTasks.map((task) => {
                            const hitBps = computeTaskHitBps(task);
                            const totalPrompt = task.usage
                              ? task.usage.inputTokens +
                                task.usage.cachedReadTokens +
                                task.usage.cachedWriteTokens
                              : 0;
                            return (
                              <tr
                                key={task.task_id}
                                className="cursor-pointer"
                                onClick={() => setSelectedTask(task)}
                              >
                                <td className="font-mono text-xs text-[var(--faint)] whitespace-nowrap">
                                  {formatTimestamp(task.started_at_unix_ms)}
                                </td>
                                <td>
                                  <div className="font-mono text-xs font-medium text-[var(--text)]">
                                    {task.task_id}
                                  </div>
                                  <div className="text-[11px] text-[var(--faint)]">
                                    {task.protocol === "openai_responses"
                                      ? "/v1/responses"
                                      : "/v1/messages"}{" "}
                                    · {task.stream ? "SSE" : "JSON"}
                                  </div>
                                </td>
                                <td className="font-mono text-xs">
                                  {task.requested_model === task.resolved_model ? (
                                    <span className="text-[var(--text-2)]">
                                      {task.resolved_model}
                                    </span>
                                  ) : (
                                    <div>
                                      <div className="text-[var(--faint)]">
                                        {task.requested_model}
                                      </div>
                                      <div className="text-[var(--text)]">
                                        → {task.resolved_model}
                                      </div>
                                    </div>
                                  )}
                                </td>
                                <td className="whitespace-nowrap">
                                  {task.session_id ? (
                                    <div className="flex items-center gap-1.5">
                                      <code className="font-mono text-xs text-[var(--text-2)]">
                                        {task.session_id.slice(0, 10)}…
                                      </code>
                                      {task.session_reused ? (
                                        <span className="sf-pill sf-pill--ok">
                                          复用
                                        </span>
                                      ) : (
                                        <span className="sf-pill">新建</span>
                                      )}
                                    </div>
                                  ) : (
                                    <span className="text-[var(--faint)]">-</span>
                                  )}
                                </td>
                                <td>
                                  <div className="flex flex-wrap items-center gap-1.5">
                                    {renderPhasePill(task.phase)}
                                    {task.client_tool_calls.length > 0 && (
                                      <span className="sf-pill sf-pill--warn">
                                        工具×{task.client_tool_calls.length}
                                      </span>
                                    )}
                                    {task.native_web_searches > 0 && (
                                      <span className="sf-pill sf-pill--info">
                                        Web×{task.native_web_searches}
                                      </span>
                                    )}
                                    {(task.native_web_fetches ?? 0) > 0 && (
                                      <span className="sf-pill sf-pill--info">
                                        Fetch×{task.native_web_fetches}
                                      </span>
                                    )}
                                    {(task.vision_offloaded_images ?? 0) > 0 && (
                                      <span className="sf-pill sf-pill--accent">
                                        Vision×{task.vision_offloaded_images}
                                      </span>
                                    )}
                                  </div>
                                </td>
                                <td className="num whitespace-nowrap">
                                  {hitBps !== null ? (
                                    <span
                                      className={`sf-pill ${
                                        hitBps < effectiveThresholdBps
                                          ? "sf-pill--err"
                                          : "sf-pill--ok"
                                      }`}
                                    >
                                      {formatBpsPct(hitBps, 1)}
                                    </span>
                                  ) : (
                                    <span className="text-[var(--faint)]">—</span>
                                  )}
                                </td>
                                <td className="num whitespace-nowrap text-xs">
                                  {task.usage ? (
                                    <div>
                                      <div className="text-[var(--text)]">
                                        {formatNumber(totalPrompt)} /{" "}
                                        {formatNumber(task.usage.outputTokens)}
                                      </div>
                                      <div className="text-[11px] text-[var(--faint)]">
                                        缓存↓{" "}
                                        {formatNumber(task.usage.cachedReadTokens)}
                                      </div>
                                    </div>
                                  ) : (
                                    "-"
                                  )}
                                </td>
                                <td className="num whitespace-nowrap text-xs">
                                  <div className="text-[var(--text)]">
                                    {formatDurationMs(task.elapsed_ms)}
                                  </div>
                                  {task.ttft_ms != null && (
                                    <div className="text-[11px] text-[var(--faint)]">
                                      TTFT {formatDurationMs(task.ttft_ms)}
                                    </div>
                                  )}
                                </td>
                              </tr>
                            );
                          })}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>
              </section>
            </>
          )}

          {/* ===============================================================
             VIEW 2: CACHE HIT RATE ANALYTICS & PERSISTED LOW-HIT RECORDS
             =============================================================== */}
          {activeTab === "cache" && (
            <>
              <header className="sf-page-header">
                <div>
                  <p className="sf-eyebrow">PROMPT CACHE · PERSISTENT AUDIT</p>
                  <h1 className="sf-title">缓存命中率计算与低命中持久化审计</h1>
                  <p className="sf-desc">
                    精确核算累计总缓存命中率与热会话续接命中率，并将低于阈值的异常请求持久化至本地磁盘（默认上限
                    1,000 条，支持在线动态调整容量与阈值）。
                  </p>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  {cacheSummary?.persisted_path && (
                    <span
                      className="sf-tag max-w-[340px] truncate"
                      title={cacheSummary.persisted_path}
                    >
                      持久化文件: {cacheSummary.persisted_path}
                    </span>
                  )}
                  <button
                    id="btn-refresh-low-cache"
                    type="button"
                    className="sf-btn sf-btn--sm"
                    onClick={() => {
                      void refreshLowCacheHits();
                    }}
                  >
                    <RefreshCw
                      className={`h-3.5 w-3.5${lowCacheLoading ? " animate-spin" : ""}`}
                    />
                    刷新记录
                  </button>
                </div>
              </header>

              {/* Cache KPIs + Composition Breakdown */}
              <div className="grid grid-cols-1 gap-4 lg:grid-cols-12">
                <section className="sf-card lg:col-span-7">
                  <div className="sf-card__header">
                    <div>
                      <h2 className="sf-card__title">累计 Prompt 缓存命中总览</h2>
                      <p className="sf-card__sub">
                        命中率公式：
                        <code>
                          cachedRead / (uncachedInput + cachedRead + cachedWrite)
                        </code>
                        （跨网关重启持久化累计）
                      </p>
                    </div>
                    <span className="sf-tag">
                      已评估 {formatNumber(overallCacheStats.evaluatedTurns)} 轮
                    </span>
                  </div>
                  <div className="sf-card__body space-y-5">
                    <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
                      <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5">
                        <div className="text-xs text-[var(--muted)]">
                          总缓存命中率（全部轮次）
                        </div>
                        <div className="mt-1.5 font-serif text-3xl text-[var(--text)] tabular-nums">
                          {formatBpsPct(overallCacheStats.hitBps, 2)}
                        </div>
                        <div className="mt-1 font-mono text-[11px] text-[var(--faint)]">
                          共 {formatNumber(overallCacheStats.evaluatedTurns)} 轮请求
                        </div>
                      </div>

                      <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5">
                        <div className="text-xs text-[var(--muted)]">
                          热会话续接命中率（复用会话）
                        </div>
                        <div className="mt-1.5 font-serif text-3xl text-[var(--ok)] tabular-nums">
                          {formatBpsPct(overallCacheStats.warmHitBps, 2)}
                        </div>
                        <div className="mt-1 font-mono text-[11px] text-[var(--faint)]">
                          复用 {formatNumber(overallCacheStats.warmTurns)} 轮 · 排除首轮冷启动
                        </div>
                      </div>

                      <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5">
                        <div className="text-xs text-[var(--muted)]">
                          累计触发低命中审计
                        </div>
                        <div className="mt-1.5 font-serif text-3xl text-[var(--primary)] tabular-nums">
                          {formatNumber(
                            cacheSummary?.total_low_cache_turns ??
                              lowCacheRecords.length,
                          )}
                        </div>
                        <div className="mt-1 font-mono text-[11px] text-[var(--faint)]">
                          当前保留 {formatNumber(storedLowCount)} /{" "}
                          {formatNumber(effectiveMaxLowRecords)} 条
                        </div>
                      </div>
                    </div>

                    {/* Stacked Token Composition Bar */}
                    <div className="space-y-2">
                      <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
                        <span className="font-medium text-[var(--text-2)]">
                          累计输入 Token 构成（总输入{" "}
                          <span className="font-mono">
                            {formatNumber(overallCacheStats.promptTokens)}
                          </span>{" "}
                          tokens）
                        </span>
                        <span className="font-mono text-[var(--faint)]">
                          节省重算{" "}
                          {formatNumber(overallCacheStats.cachedRead)} tokens
                        </span>
                      </div>

                      <div className="flex h-3 w-full overflow-hidden rounded-full bg-[var(--surface-sunk)]">
                        {overallCacheStats.promptTokens > 0 && (
                          <>
                            <div
                              style={{
                                width: `${(overallCacheStats.cachedRead / overallCacheStats.promptTokens) * 100}%`,
                                background: "var(--ok)",
                              }}
                              title={`缓存命中读取: ${formatNumber(overallCacheStats.cachedRead)}`}
                            />
                            <div
                              style={{
                                width: `${(overallCacheStats.cachedWrite / overallCacheStats.promptTokens) * 100}%`,
                                background: "var(--warn)",
                              }}
                              title={`缓存写入: ${formatNumber(overallCacheStats.cachedWrite)}`}
                            />
                            <div
                              style={{
                                width: `${(overallCacheStats.uncachedInput / overallCacheStats.promptTokens) * 100}%`,
                                background: "var(--primary)",
                              }}
                              title={`未命中输入: ${formatNumber(overallCacheStats.uncachedInput)}`}
                            />
                          </>
                        )}
                      </div>

                      <div className="flex flex-wrap items-center gap-x-5 gap-y-1.5 pt-1 font-mono text-xs text-[var(--muted)]">
                        <span className="inline-flex items-center gap-1.5">
                          <span className="h-2.5 w-2.5 rounded-[3px] bg-[var(--ok)]" />
                          缓存读 (Hit):{" "}
                          <strong className="text-[var(--text)]">
                            {formatNumber(overallCacheStats.cachedRead)}
                          </strong>
                        </span>
                        <span className="inline-flex items-center gap-1.5">
                          <span className="h-2.5 w-2.5 rounded-[3px] bg-[var(--warn)]" />
                          缓存写 (Write):{" "}
                          <strong className="text-[var(--text)]">
                            {formatNumber(overallCacheStats.cachedWrite)}
                          </strong>
                        </span>
                        <span className="inline-flex items-center gap-1.5">
                          <span className="h-2.5 w-2.5 rounded-[3px] bg-[var(--primary)]" />
                          未命中输入 (Uncached):{" "}
                          <strong className="text-[var(--text)]">
                            {formatNumber(overallCacheStats.uncachedInput)}
                          </strong>
                        </span>
                      </div>
                    </div>
                  </div>
                </section>

                {/* Live Persistence Config Form */}
                <section className="sf-card lg:col-span-5">
                  <div className="sf-card__header">
                    <div>
                      <h2 className="sf-card__title">低命中记录持久化配置</h2>
                      <p className="sf-card__sub">
                        在线修改实时生效并原子同步至{" "}
                        <code>settings.json</code> 与{" "}
                        <code>cache-telemetry.json</code>
                      </p>
                    </div>
                  </div>
                  <form
                    className="sf-card__body space-y-4"
                    onSubmit={(ev) => {
                      void handleSaveCacheConfig(ev);
                    }}
                  >
                    <div className="space-y-1.5">
                      <div className="flex items-center justify-between">
                        <label
                          htmlFor="input-max-low-cache-records"
                          className="text-xs font-medium text-[var(--text-2)]"
                        >
                          最大持久化记录条数（默认 1000，设为 0 关闭）
                        </label>
                        <div className="flex gap-1">
                          {[200, 1000, 5000].map((preset) => (
                            <button
                              key={preset}
                              type="button"
                              className="sf-tag hover:border-[var(--primary)] hover:text-[var(--primary)] cursor-pointer"
                              onClick={() => {
                                setDraftMaxRecords(String(preset));
                                setCacheConfigDirty(true);
                              }}
                            >
                              {preset}
                            </button>
                          ))}
                        </div>
                      </div>
                      <input
                        id="input-max-low-cache-records"
                        type="number"
                        min={0}
                        max={50000}
                        className="sf-input font-mono"
                        value={draftMaxRecords}
                        onChange={(e) => {
                          setDraftMaxRecords(e.target.value);
                          setCacheConfigDirty(true);
                        }}
                      />
                    </div>

                    <div className="space-y-1.5">
                      <div className="flex items-center justify-between">
                        <label
                          htmlFor="input-low-cache-threshold-pct"
                          className="text-xs font-medium text-[var(--text-2)]"
                        >
                          低缓存命中率判定阈值 %（低于此百分比即持久化）
                        </label>
                        <div className="flex gap-1">
                          {[30, 50, 70, 85].map((pct) => (
                            <button
                              key={pct}
                              type="button"
                              className="sf-tag hover:border-[var(--primary)] hover:text-[var(--primary)] cursor-pointer"
                              onClick={() => {
                                setDraftThresholdPct(String(pct));
                                setCacheConfigDirty(true);
                              }}
                            >
                              {pct}%
                            </button>
                          ))}
                        </div>
                      </div>
                      <input
                        id="input-low-cache-threshold-pct"
                        type="number"
                        min={0}
                        max={100}
                        className="sf-input font-mono"
                        value={draftThresholdPct}
                        onChange={(e) => {
                          setDraftThresholdPct(e.target.value);
                          setCacheConfigDirty(true);
                        }}
                      />
                    </div>

                    {cacheConfigBanner && (
                      <div
                        className={`sf-banner ${
                          cacheConfigBanner.tone === "ok"
                            ? "sf-banner--ok"
                            : "sf-banner--err"
                        }`}
                      >
                        <span>{cacheConfigBanner.text}</span>
                      </div>
                    )}

                    <div className="flex flex-wrap items-center justify-between gap-2 pt-1">
                      <button
                        id="btn-save-cache-config"
                        type="submit"
                        disabled={cacheConfigSaving}
                        className="sf-btn sf-btn--primary"
                      >
                        {cacheConfigSaving ? "保存中…" : "保存持久化策略"}
                      </button>

                      {confirmClearMode === null ? (
                        <div className="flex items-center gap-1.5">
                          <button
                            id="btn-clear-low-cache"
                            type="button"
                            className="sf-btn sf-btn--danger sf-btn--sm"
                            disabled={storedLowCount === 0}
                            onClick={() => setConfirmClearMode("records")}
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                            清空记录
                          </button>
                          <button
                            id="btn-reset-cache-totals"
                            type="button"
                            className="sf-btn sf-btn--ghost sf-btn--sm"
                            onClick={() => setConfirmClearMode("all")}
                          >
                            重置累计统计
                          </button>
                        </div>
                      ) : (
                        <div className="flex items-center gap-1.5">
                          <button
                            type="button"
                            className="sf-btn sf-btn--danger sf-btn--sm"
                            onClick={() => {
                              void handleConfirmClear(confirmClearMode === "all");
                            }}
                          >
                            确认{confirmClearMode === "all" ? "重置全部" : "清空记录"}
                          </button>
                          <button
                            type="button"
                            className="sf-btn sf-btn--sm"
                            onClick={() => setConfirmClearMode(null)}
                          >
                            取消
                          </button>
                        </div>
                      )}
                    </div>
                  </form>
                </section>
              </div>

              {/* Persisted Low Cache Hit Records Table */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      已持久化的低缓存命中率记录（{formatNumber(filteredLowCacheRecords.length)}{" "}
                      / 上限 {formatNumber(effectiveMaxLowRecords)} 条）
                    </h2>
                    <p className="sf-card__sub">
                      超出容量上限时按环形队列自动淘汰最旧记录；点击任意行可查看根因归类与优化建议
                    </p>
                  </div>

                  <div className="flex flex-wrap items-center gap-2">
                    <div className="sf-seg" role="tablist" aria-label="低命中原因筛选">
                      <button
                        type="button"
                        className={`sf-seg__btn${reasonFilter === "all" ? " is-active" : ""}`}
                        onClick={() => {
                          setReasonFilter("all");
                          setCachePage(1);
                        }}
                      >
                        全部 ({lowCacheRecords.length})
                      </button>
                      <button
                        type="button"
                        className={`sf-seg__btn${reasonFilter === "warm_cache_miss" ? " is-active" : ""}`}
                        onClick={() => {
                          setReasonFilter("warm_cache_miss");
                          setCachePage(1);
                        }}
                      >
                        热掉缓存 ({reasonCounts.warmMiss})
                      </button>
                      <button
                        type="button"
                        className={`sf-seg__btn${reasonFilter === "warm_partial_hit" ? " is-active" : ""}`}
                        onClick={() => {
                          setReasonFilter("warm_partial_hit");
                          setCachePage(1);
                        }}
                      >
                        部分命中 ({reasonCounts.warmPartial})
                      </button>
                      <button
                        type="button"
                        className={`sf-seg__btn${reasonFilter === "cold_session_start" ? " is-active" : ""}`}
                        onClick={() => {
                          setReasonFilter("cold_session_start");
                          setCachePage(1);
                        }}
                      >
                        冷启动 ({reasonCounts.coldStart})
                      </button>
                    </div>

                    <div className="relative w-56">
                      <Search className="pointer-events-none absolute left-2.5 top-2.5 h-3.5 w-3.5 text-[var(--faint)]" />
                      <input
                        id="input-search-low-cache"
                        type="search"
                        placeholder="搜索模型 / 会话 / Prompt…"
                        className="sf-input pl-8 text-xs"
                        value={cacheQuery}
                        onChange={(e) => {
                          setCacheQuery(e.target.value);
                          setCachePage(1);
                        }}
                      />
                    </div>
                  </div>
                </div>

                <div className="sf-card__body sf-card__body--flush">
                  {pagedLowCacheRecords.length === 0 ? (
                    <div className="sf-empty">
                      <p className="sf-empty__title">
                        {lowCacheRecords.length === 0
                          ? "当前暂无低于阈值的缓存未命中记录"
                          : "没有匹配当前筛选条件的低命中记录"}
                      </p>
                      <p className="sf-empty__hint">
                        {lowCacheRecords.length === 0
                          ? `当任意已完成请求的缓存命中率低于 ${Math.round(effectiveThresholdBps / 100)}% 时，网关将自动原子持久化记录至此列表。`
                          : "请尝试切换上方原因分类标签或清空搜索关键词。"}
                      </p>
                    </div>
                  ) : (
                    <div className="sf-table-wrap">
                      <table className="sf-table">
                        <thead>
                          <tr>
                            <th>记录时间</th>
                            <th>诊断归因</th>
                            <th>命中率</th>
                            <th>模型与会话</th>
                            <th className="num">缓存读 / 总输入</th>
                            <th className="num">未命中输入</th>
                            <th className="num">TTFT / 总耗时</th>
                            <th>Prompt 摘要</th>
                          </tr>
                        </thead>
                        <tbody>
                          {pagedLowCacheRecords.map((rec, idx) => (
                            <tr
                              key={`${rec.task_id}-${rec.recorded_at_unix_ms}-${idx}`}
                              className="cursor-pointer"
                              onClick={() => setSelectedLowRecord(rec)}
                            >
                              <td className="font-mono text-xs text-[var(--faint)] whitespace-nowrap">
                                {formatDateTime(rec.recorded_at_unix_ms)}
                              </td>
                              <td className="whitespace-nowrap">
                                {renderReasonPill(rec.reason)}
                              </td>
                              <td className="whitespace-nowrap">
                                <div className="flex items-center gap-2">
                                  <span className="font-mono text-xs font-semibold text-[var(--err)] tabular-nums w-12">
                                    {formatBpsPct(rec.cache_hit_rate_bps, 1)}
                                  </span>
                                  <div className="sf-progress w-16">
                                    <span
                                      style={{
                                        width: `${Math.min(100, Math.round(rec.cache_hit_rate_bps / 100))}%`,
                                        background:
                                          rec.cache_hit_rate_bps === 0
                                            ? "var(--err)"
                                            : "var(--warn)",
                                      }}
                                    />
                                  </div>
                                </div>
                              </td>
                              <td>
                                <div className="font-mono text-xs text-[var(--text)]">
                                  {rec.resolved_model}
                                </div>
                                <div className="font-mono text-[11px] text-[var(--faint)]">
                                  {rec.session_id
                                    ? `${rec.session_reused ? "复用" : "新建"} · ${rec.session_id.slice(0, 10)}…`
                                    : rec.task_id}
                                </div>
                              </td>
                              <td className="num whitespace-nowrap text-xs">
                                <span className="text-[var(--ok)]">
                                  {formatNumber(rec.cached_read_tokens)}
                                </span>{" "}
                                / {formatNumber(rec.total_prompt_tokens)}
                              </td>
                              <td className="num whitespace-nowrap text-xs text-[var(--primary)]">
                                {formatNumber(rec.input_uncached_tokens)}
                              </td>
                              <td className="num whitespace-nowrap text-xs">
                                <div>{formatDurationMs(rec.elapsed_ms)}</div>
                                {rec.ttft_ms != null && (
                                  <div className="text-[11px] text-[var(--faint)]">
                                    TTFT {formatDurationMs(rec.ttft_ms)}
                                  </div>
                                )}
                              </td>
                              <td className="max-w-[260px]">
                                <span className="block truncate text-xs text-[var(--muted)]">
                                  {rec.prompt_preview || "-"}
                                </span>
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>

                {filteredLowCacheRecords.length > LOW_CACHE_PAGE_SIZE && (
                  <div className="sf-card__footer">
                    <span className="font-mono text-xs tabular-nums">
                      第 {clampedCachePage} / {totalLowCachePages} 页 · 共{" "}
                      {formatNumber(filteredLowCacheRecords.length)} 条记录
                    </span>
                    <div className="flex items-center gap-1.5">
                      <button
                        type="button"
                        className="sf-btn sf-btn--sm"
                        disabled={clampedCachePage <= 1}
                        onClick={() =>
                          setCachePage((p) => Math.max(1, p - 1))
                        }
                      >
                        <ChevronLeft className="h-3.5 w-3.5" /> 上一页
                      </button>
                      <button
                        type="button"
                        className="sf-btn sf-btn--sm"
                        disabled={clampedCachePage >= totalLowCachePages}
                        onClick={() =>
                          setCachePage((p) =>
                            Math.min(totalLowCachePages, p + 1),
                          )
                        }
                      >
                        下一页 <ChevronRight className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  </div>
                )}
              </section>
            </>
          )}

          {/* ===============================================================
             VIEW 3: ACP SESSION POOL & MASTER PROCESS SUPERVISION
             =============================================================== */}
          {activeTab === "sessions" && (
            <>
              <header className="sf-page-header">
                <div>
                  <p className="sf-eyebrow">INFRASTRUCTURE · ACP SUPERVISION</p>
                  <h1 className="sf-title">ACP 会话池与跨平台进程监管</h1>
                  <p className="sf-desc">
                    监管 <code>agy acp --process-per-session</code> 主控与独立工作子进程树，支持热会话前缀匹配、45
                    分钟冷空闲自动回收及 &ge;150k 长上下文视觉旁路卸载热开关。
                  </p>
                </div>
                {acp && (
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="sf-tag">
                      {acp.os} / {acp.arch}
                    </span>
                    <span className="sf-tag">Protocol v{acp.protocol_version}</span>
                    {acp.agent_version && (
                      <span className="sf-tag">agy v{acp.agent_version}</span>
                    )}
                  </div>
                )}
              </header>

              {/* Master Process Overview Card */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      {acp?.agent_name ?? "Antigravity ACP Server"} 主进程监管与运行策略
                    </h2>
                    <p className="sf-card__sub">
                      隔离策略：{acp?.supervision_strategy ?? "-"}
                    </p>
                  </div>

                  <div className="flex flex-wrap items-center gap-2">
                    <span
                      className={`sf-pill ${
                        (snapshot?.vision_offload_enabled ?? true)
                          ? "sf-pill--ok"
                          : ""
                      }`}
                    >
                      &ge;150k 视觉子会话卸载:{" "}
                      {(snapshot?.vision_offload_enabled ?? true)
                        ? "已开启"
                        : "已关闭"}
                    </span>
                    <button
                      id="btn-toggle-vision-offload"
                      type="button"
                      className="sf-btn sf-btn--sm"
                      onClick={() => {
                        const current = snapshot?.vision_offload_enabled ?? true;
                        void toggleVisionOffload(!current);
                      }}
                    >
                      {(snapshot?.vision_offload_enabled ?? true)
                        ? "关闭子会话卸载"
                        : "开启子会话卸载"}
                    </button>
                  </div>
                </div>

                <div className="sf-card__body space-y-4">
                  <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
                    <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3">
                      <div className="text-xs text-[var(--muted)]">
                        JSON-RPC 请求 / 响应
                      </div>
                      <div className="mt-1 font-mono text-sm font-semibold text-[var(--text)] tabular-nums">
                        {formatNumber(acp?.rpc_requests_sent ?? 0)} /{" "}
                        {formatNumber(acp?.rpc_responses_received ?? 0)}
                      </div>
                    </div>
                    <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3">
                      <div className="text-xs text-[var(--muted)]">
                        流式事件通知帧
                      </div>
                      <div className="mt-1 font-mono text-sm font-semibold text-[var(--text)] tabular-nums">
                        {formatNumber(acp?.rpc_notifications_received ?? 0)}
                      </div>
                    </div>
                    <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3">
                      <div className="text-xs text-[var(--muted)]">
                        挂起 RPC / 活跃流通道
                      </div>
                      <div className="mt-1 font-mono text-sm font-semibold text-[var(--text)] tabular-nums">
                        {acp?.rpc_pending_in_flight ?? 0} /{" "}
                        {acp?.active_prompt_streams ?? 0}
                      </div>
                    </div>
                    <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3">
                      <div className="text-xs text-[var(--muted)]">
                        默认模型 / 模式
                      </div>
                      <div className="mt-1 truncate font-mono text-xs font-semibold text-[var(--text)]">
                        {acp?.default_model ?? "-"} · {acp?.acp_mode ?? "-"}
                      </div>
                    </div>
                  </div>

                  {acp && (
                    <dl className="grid grid-cols-1 gap-3 rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5 text-xs md:grid-cols-2">
                      <div className="min-w-0">
                        <dt className="text-[var(--faint)]">AGY 可执行文件</dt>
                        <dd className="mt-0.5 truncate font-mono text-[var(--text-2)]">
                          {acp.binary_path}
                        </dd>
                      </div>
                      <div className="min-w-0">
                        <dt className="text-[var(--faint)]">隔离工作目录 (CWD)</dt>
                        <dd className="mt-0.5 truncate font-mono text-[var(--text-2)]">
                          {acp.session_cwd}
                        </dd>
                      </div>
                      <div className="min-w-0">
                        <dt className="text-[var(--faint)]">出站代理策略</dt>
                        <dd className="mt-0.5 truncate font-mono text-[var(--text-2)]">
                          {acp.agy_proxy
                            ? `强制代理 (${acp.agy_proxy})`
                            : "直连 (无注入代理)"}
                        </dd>
                      </div>
                      <div className="min-w-0">
                        <dt className="text-[var(--faint)]">AGY 空闲自动热更新</dt>
                        <dd className="mt-0.5 truncate font-mono text-[var(--text-2)]">
                          {(acp.auto_update_enabled ?? true) ? "开启" : "关闭"}
                          {acp.latest_known_version
                            ? ` · 上游 v${acp.latest_known_version}`
                            : ""}
                          {acp.auto_update_note ? ` (${acp.auto_update_note})` : ""}
                        </dd>
                      </div>
                    </dl>
                  )}
                </div>
              </section>

              {/* Session Pool Workers Table */}
              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      池化 ACP 会话子进程（{pool?.sessions.length ?? 0} 个驻留 · 累计淘汰{" "}
                      {pool?.total_sessions_closed_or_evicted ?? 0} 个）
                    </h2>
                    <p className="sf-card__sub">
                      展示各会话的实时上游 KV 缓存温区、单会话实测缓存命中率与上下文窗口占用
                    </p>
                  </div>
                </div>
                <div className="sf-card__body sf-card__body--flush">
                  {!pool || pool.sessions.length === 0 ? (
                    <div className="sf-empty">
                      <p className="sf-empty__title">当前会话池为空</p>
                      <p className="sf-empty__hint">
                        首个多轮对话到达后将自动创建并驻留独立 ACP 子进程供后续轮次热复用。
                      </p>
                    </div>
                  ) : (
                    <div className="sf-table-wrap">
                      <table className="sf-table">
                        <thead>
                          <tr>
                            <th>Session ID</th>
                            <th>状态 / KV 温区</th>
                            <th>绑定模型</th>
                            <th className="num">单会话命中率</th>
                            <th className="num">上下文占用</th>
                            <th className="num">轮次 / 消息数</th>
                            <th className="num">最近活跃</th>
                            <th className="num">操作</th>
                          </tr>
                        </thead>
                        <tbody>
                          {pool.sessions.map((sess) => (
                            <tr key={sess.session_id}>
                              <td>
                                <div className="flex items-center gap-1">
                                  <code className="font-mono text-xs font-medium text-[var(--text)]">
                                    {sess.session_id}
                                  </code>
                                  <CopyButton
                                    value={sess.session_id}
                                    label="复制 Session ID"
                                    compact
                                  />
                                </div>
                              </td>
                              <td className="whitespace-nowrap">
                                {sess.state === "running" ? (
                                  <span className="sf-pill sf-pill--accent sf-pill--live">
                                    执行中 (Leased)
                                  </span>
                                ) : sess.cache_warmth === "hot" ? (
                                  <span className="sf-pill sf-pill--ok">
                                    热缓存 (&lt;1m)
                                  </span>
                                ) : sess.cache_warmth === "warm" ? (
                                  <span className="sf-pill sf-pill--warn">
                                    温缓存 (1–5m)
                                  </span>
                                ) : (
                                  <span className="sf-pill">
                                    冷驻留 (&ge;5m)
                                  </span>
                                )}
                              </td>
                              <td className="font-mono text-xs text-[var(--text-2)]">
                                {sess.model_id}
                              </td>
                              <td className="num whitespace-nowrap">
                                {sess.cache_hit_rate_bps != null ? (
                                  <span
                                    className={`sf-pill ${
                                      sess.cache_hit_rate_bps <
                                      effectiveThresholdBps
                                        ? "sf-pill--warn"
                                        : "sf-pill--ok"
                                    }`}
                                  >
                                    {formatBpsPct(sess.cache_hit_rate_bps, 1)}
                                  </span>
                                ) : (
                                  <span className="text-[var(--faint)]">—</span>
                                )}
                              </td>
                              <td className="num font-mono text-xs">
                                {(sess.last_context_tokens ?? 0) > 0
                                  ? formatNumber(sess.last_context_tokens ?? 0)
                                  : "-"}
                              </td>
                              <td className="num font-mono text-xs">
                                {sess.turn_count} / {sess.message_count}
                              </td>
                              <td className="num font-mono text-xs text-[var(--faint)]">
                                {formatTimestamp(sess.last_active_unix_ms)}
                              </td>
                              <td className="num">
                                {sess.state === "idle" && (
                                  <button
                                    type="button"
                                    className="sf-btn sf-btn--danger sf-btn--sm"
                                    onClick={() => {
                                      void closeIdleSession(sess.session_id);
                                    }}
                                  >
                                    <Trash2 className="h-3 w-3" />
                                    释放
                                  </button>
                                )}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>
              </section>
            </>
          )}

          {/* ===============================================================
             VIEW 4: AVAILABLE MODELS CATALOG
             =============================================================== */}
          {activeTab === "models" && (
            <>
              <header className="sf-page-header">
                <div>
                  <p className="sf-eyebrow">CATALOG · LIVE DISCOVERY</p>
                  <h1 className="sf-title">
                    可用 Antigravity 模型目录（共 {models.length} 个）
                  </h1>
                  <p className="sf-desc">
                    启动时从 <code>agy acp</code> 会话握手实时同步，支持精确模型 ID 直连以及
                    Codex / Claude Code 别名自适应映射。
                  </p>
                </div>
              </header>

              <section className="sf-card">
                <div className="sf-card__header">
                  <div className="sf-seg" role="tablist" aria-label="模型家族筛选">
                    <button
                      type="button"
                      className={`sf-seg__btn${modelFamily === "all" ? " is-active" : ""}`}
                      onClick={() => setModelFamily("all")}
                    >
                      全部 ({models.length})
                    </button>
                    <button
                      type="button"
                      className={`sf-seg__btn${modelFamily === "gemini" ? " is-active" : ""}`}
                      onClick={() => setModelFamily("gemini")}
                    >
                      Gemini 系列
                    </button>
                    <button
                      type="button"
                      className={`sf-seg__btn${modelFamily === "claude" ? " is-active" : ""}`}
                      onClick={() => setModelFamily("claude")}
                    >
                      Claude 系列
                    </button>
                    <button
                      type="button"
                      className={`sf-seg__btn${modelFamily === "other" ? " is-active" : ""}`}
                      onClick={() => setModelFamily("other")}
                    >
                      其他模型
                    </button>
                  </div>

                  <div className="relative w-64">
                    <Search className="pointer-events-none absolute left-2.5 top-2.5 h-3.5 w-3.5 text-[var(--faint)]" />
                    <input
                      id="input-search-models"
                      type="search"
                      placeholder="搜索模型 ID 或展示名…"
                      className="sf-input pl-8 text-xs"
                      value={modelQuery}
                      onChange={(e) => setModelQuery(e.target.value)}
                    />
                  </div>
                </div>

                <div className="sf-card__body">
                  {filteredModels.length === 0 ? (
                    <div className="sf-empty">
                      <p className="sf-empty__title">未找到匹配的模型</p>
                      <p className="sf-empty__hint">
                        请调整搜索关键词，或检查网关是否已完成与 <code>agy acp</code> 的初始握手。
                      </p>
                    </div>
                  ) : (
                    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
                      {filteredModels.map((m) => (
                        <div
                          key={m.modelId}
                          className="flex items-center justify-between gap-3 rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] px-3.5 py-3 transition-colors hover:border-[var(--border-strong)]"
                        >
                          <div className="min-w-0">
                            <div className="flex items-center gap-1">
                              <code className="truncate font-mono text-xs font-semibold text-[var(--text)]">
                                {m.modelId}
                              </code>
                              <CopyButton
                                value={m.modelId}
                                label="复制模型 ID"
                                compact
                              />
                            </div>
                            <div className="mt-0.5 truncate text-xs text-[var(--muted)]">
                              {m.name || m.modelId}
                            </div>
                          </div>
                          <button
                            type="button"
                            className="sf-btn sf-btn--sm shrink-0"
                            onClick={() => {
                              setProbeModel(m.modelId);
                              setProbeOpen(true);
                            }}
                          >
                            实测
                          </button>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </section>
            </>
          )}

          {/* ===============================================================
             VIEW 5: DIAGNOSTICS STDERR & QUICK INTEGRATION SNIPPETS
             =============================================================== */}
          {activeTab === "logs" && (
            <>
              <header className="sf-page-header">
                <div>
                  <p className="sf-eyebrow">DIAGNOSTICS · HARNESS INTEGRATION</p>
                  <h1 className="sf-title">进程诊断日志与客户端接入</h1>
                  <p className="sf-desc">
                    查看受监管 <code>agy acp</code> 子进程最近 100 行{" "}
                    <code>stderr</code> 输出，以及 Codex CLI / Claude Code 快速接入方式。
                  </p>
                </div>
              </header>

              <section className="sf-card">
                <div className="sf-card__header">
                  <div>
                    <h2 className="sf-card__title">
                      ACP 子进程诊断日志（stderr · {acp?.recent_stderr.length ?? 0} 行）
                    </h2>
                    <p className="sf-card__sub">
                      用于定位底层认证状态、上游代理连接或子进程异常输出
                    </p>
                  </div>
                  {acp && acp.recent_stderr.length > 0 && (
                    <CopyButton
                      value={acp.recent_stderr
                        .map(
                          (e) =>
                            `[${formatTimestamp(e.timestamp_unix_ms)}] ${e.line}`,
                        )
                        .join("\n")}
                      label="复制日志"
                    />
                  )}
                </div>
                <div className="sf-card__body">
                  {!acp || acp.recent_stderr.length === 0 ? (
                    <div className="sf-banner sf-banner--ok font-mono text-xs">
                      <CheckCircle2 className="h-4 w-4 shrink-0" />
                      <span>[healthy] agy acp stderr 干净，无异常告警输出</span>
                    </div>
                  ) : (
                    <pre className="sf-code max-h-80">
                      {acp.recent_stderr
                        .map(
                          (entry) =>
                            `[${formatTimestamp(entry.timestamp_unix_ms)}] ${entry.line}`,
                        )
                        .join("\n")}
                    </pre>
                  )}
                </div>
              </section>

              <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
                <section className="sf-card">
                  <div className="sf-card__header">
                    <div>
                      <h2 className="sf-card__title">OpenAI Codex CLI 接入</h2>
                      <p className="sf-card__sub">
                        推荐使用 <code>lg cx</code> 临时会话级注入，零全局配置污染
                      </p>
                    </div>
                    <CopyButton value={codexConfigSnippet} label="复制配置" />
                  </div>
                  <div className="sf-card__body">
                    <pre className="sf-code">{codexConfigSnippet}</pre>
                  </div>
                </section>

                <section className="sf-card">
                  <div className="sf-card__header">
                    <div>
                      <h2 className="sf-card__title">Claude Code 接入</h2>
                      <p className="sf-card__sub">
                        推荐使用 <code>lg cc</code> 启动，或手动配置{" "}
                        <code>ANTHROPIC_BASE_URL</code>
                      </p>
                    </div>
                    <CopyButton value={claudeEnvSnippet} label="复制命令" />
                  </div>
                  <div className="sf-card__body">
                    <pre className="sf-code">{claudeEnvSnippet}</pre>
                  </div>
                </section>
              </div>
            </>
          )}
        </main>
      </div>

      {/* =================================================================
         DIALOG 1: TASK DETAIL INSPECTION
         ================================================================= */}
      <Dialog
        open={selectedTask !== null}
        onOpenChange={(open) => {
          if (!open) setSelectedTask(null);
        }}
      >
        <DialogContent>
          {selectedTask && (
            <>
              <DialogHeader>
                <DialogTitle className="flex flex-wrap items-center gap-2 font-mono text-base">
                  <span>{selectedTask.task_id}</span>
                  {renderPhasePill(selectedTask.phase)}
                </DialogTitle>
                <DialogDescription>
                  {selectedTask.protocol === "openai_responses"
                    ? "OpenAI Responses API (/v1/responses)"
                    : "Anthropic Messages API (/v1/messages)"}{" "}
                  · 首字延迟{" "}
                  {selectedTask.ttft_ms != null
                    ? formatDurationMs(selectedTask.ttft_ms)
                    : "-"}{" "}
                  · 总耗时 {formatDurationMs(selectedTask.elapsed_ms)}
                </DialogDescription>
              </DialogHeader>

              <div className="grid grid-cols-1 gap-2.5 rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5 text-xs sm:grid-cols-2">
                <div>
                  <span className="text-[var(--faint)]">请求模型: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedTask.requested_model}
                  </code>
                </div>
                <div>
                  <span className="text-[var(--faint)]">实际路由: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedTask.resolved_model}
                  </code>
                </div>
                <div>
                  <span className="text-[var(--faint)]">ACP 会话: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedTask.session_id ?? "-"}
                  </code>
                </div>
                <div>
                  <span className="text-[var(--faint)]">停止原因: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedTask.stop_reason ?? "-"}
                  </code>
                </div>
              </div>

              {selectedTask.usage && (
                <div className="rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5 text-xs space-y-2">
                  <div className="flex items-center justify-between">
                    <span className="font-medium text-[var(--text)]">
                      Token 与 Prompt 缓存明细
                    </span>
                    <span className="sf-pill sf-pill--ok">
                      命中率 {formatBpsPct(computeTaskHitBps(selectedTask), 2)}
                    </span>
                  </div>
                  <div className="grid grid-cols-2 gap-2 font-mono text-[var(--text-2)] sm:grid-cols-3">
                    <span>
                      未命中输入: {formatNumber(selectedTask.usage.inputTokens)}
                    </span>
                    <span>
                      缓存读:{" "}
                      <strong className="text-[var(--ok)]">
                        {formatNumber(selectedTask.usage.cachedReadTokens)}
                      </strong>
                    </span>
                    <span>
                      缓存写: {formatNumber(selectedTask.usage.cachedWriteTokens)}
                    </span>
                    <span>
                      输出: {formatNumber(selectedTask.usage.outputTokens)}
                    </span>
                    <span>
                      思考: {formatNumber(selectedTask.usage.thoughtTokens)}
                    </span>
                    <span>
                      总计: {formatNumber(selectedTask.usage.totalTokens)}
                    </span>
                  </div>
                </div>
              )}

              {(selectedTask.native_web_searches > 0 ||
                (selectedTask.native_web_fetches ?? 0) > 0 ||
                (selectedTask.vision_offloaded_images ?? 0) > 0 ||
                selectedTask.client_tool_calls.length > 0) && (
                <div className="flex flex-wrap gap-2 text-xs">
                  {selectedTask.native_web_searches > 0 && (
                    <span className="sf-pill sf-pill--info">
                      原生联网搜索 ×{selectedTask.native_web_searches}
                    </span>
                  )}
                  {(selectedTask.native_web_fetches ?? 0) > 0 && (
                    <span className="sf-pill sf-pill--info">
                      原生网页抓取 ×{selectedTask.native_web_fetches}
                    </span>
                  )}
                  {(selectedTask.vision_offloaded_images ?? 0) > 0 && (
                    <span className="sf-pill sf-pill--accent">
                      视觉子会话旁路卸载 ×{selectedTask.vision_offloaded_images}
                    </span>
                  )}
                  {selectedTask.client_tool_calls.length > 0 && (
                    <span className="sf-pill sf-pill--warn">
                      客户端工具: {selectedTask.client_tool_calls.join(", ")}
                    </span>
                  )}
                </div>
              )}

              {selectedTask.prompt_preview && (
                <div className="space-y-1">
                  <div className="text-xs font-medium text-[var(--muted)]">
                    输入摘要
                  </div>
                  <pre className="sf-code">{selectedTask.prompt_preview}</pre>
                </div>
              )}

              {selectedTask.output_preview && (
                <div className="space-y-1">
                  <div className="text-xs font-medium text-[var(--muted)]">
                    输出摘要
                  </div>
                  <pre className="sf-code">{selectedTask.output_preview}</pre>
                </div>
              )}

              {selectedTask.error_message && (
                <div className="sf-banner sf-banner--err font-mono text-xs">
                  {selectedTask.error_message}
                </div>
              )}
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* =================================================================
         DIALOG 2: PERSISTED LOW CACHE HIT RECORD DIAGNOSTIC DETAIL
         ================================================================= */}
      <Dialog
        open={selectedLowRecord !== null}
        onOpenChange={(open) => {
          if (!open) setSelectedLowRecord(null);
        }}
      >
        <DialogContent>
          {selectedLowRecord && (
            <>
              <DialogHeader>
                <DialogTitle className="flex flex-wrap items-center gap-2">
                  <span>低缓存命中诊断详情</span>
                  {renderReasonPill(selectedLowRecord.reason)}
                  <span className="sf-pill sf-pill--err font-mono">
                    {formatBpsPct(selectedLowRecord.cache_hit_rate_bps, 2)}
                  </span>
                </DialogTitle>
                <DialogDescription>
                  任务 <code>{selectedLowRecord.task_id}</code> · 记录于{" "}
                  {formatDateTime(selectedLowRecord.recorded_at_unix_ms)}
                </DialogDescription>
              </DialogHeader>

              <div className="sf-banner text-xs">
                <span>{explainLowCacheReason(selectedLowRecord)}</span>
              </div>

              <div className="grid grid-cols-2 gap-2.5 rounded-[8px] border border-[var(--border)] bg-[var(--surface-alt)] p-3.5 text-xs sm:grid-cols-3 font-mono">
                <div>
                  <div className="font-sans text-[var(--faint)]">总输入 Prompt</div>
                  <div className="mt-0.5 text-sm font-semibold text-[var(--text)]">
                    {formatNumber(selectedLowRecord.total_prompt_tokens)}
                  </div>
                </div>
                <div>
                  <div className="font-sans text-[var(--faint)]">缓存命中读取</div>
                  <div className="mt-0.5 text-sm font-semibold text-[var(--ok)]">
                    {formatNumber(selectedLowRecord.cached_read_tokens)}
                  </div>
                </div>
                <div>
                  <div className="font-sans text-[var(--faint)]">未命中输入</div>
                  <div className="mt-0.5 text-sm font-semibold text-[var(--primary)]">
                    {formatNumber(selectedLowRecord.input_uncached_tokens)}
                  </div>
                </div>
                <div>
                  <div className="font-sans text-[var(--faint)]">缓存写入</div>
                  <div className="mt-0.5 text-[var(--text-2)]">
                    {formatNumber(selectedLowRecord.cached_write_tokens)}
                  </div>
                </div>
                <div>
                  <div className="font-sans text-[var(--faint)]">输出 Tokens</div>
                  <div className="mt-0.5 text-[var(--text-2)]">
                    {formatNumber(selectedLowRecord.output_tokens)}
                  </div>
                </div>
                <div>
                  <div className="font-sans text-[var(--faint)]">TTFT / 总耗时</div>
                  <div className="mt-0.5 text-[var(--text-2)]">
                    {selectedLowRecord.ttft_ms != null
                      ? formatDurationMs(selectedLowRecord.ttft_ms)
                      : "-"}{" "}
                    / {formatDurationMs(selectedLowRecord.elapsed_ms)}
                  </div>
                </div>
              </div>

              <div className="grid grid-cols-1 gap-2 text-xs sm:grid-cols-2">
                <div>
                  <span className="text-[var(--faint)]">路由模型: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedLowRecord.requested_model ===
                    selectedLowRecord.resolved_model
                      ? selectedLowRecord.resolved_model
                      : `${selectedLowRecord.requested_model} → ${selectedLowRecord.resolved_model}`}
                  </code>
                </div>
                <div>
                  <span className="text-[var(--faint)]">绑定会话: </span>
                  <code className="font-mono text-[var(--text)]">
                    {selectedLowRecord.session_id ?? "-"} (
                    {selectedLowRecord.session_reused ? "复用会话" : "新建会话"})
                  </code>
                </div>
              </div>

              {selectedLowRecord.prompt_preview && (
                <div className="space-y-1">
                  <div className="text-xs font-medium text-[var(--muted)]">
                    该轮输入摘要
                  </div>
                  <pre className="sf-code">{selectedLowRecord.prompt_preview}</pre>
                </div>
              )}
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* =================================================================
         DIALOG 3: GATEWAY CONNECTION SETTINGS
         ================================================================= */}
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>网关连接设置</DialogTitle>
            <DialogDescription>
              默认自动连接同源网关；若网关监听在其他端口或开启了密钥鉴权，可在此指定。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3.5">
            <div className="space-y-1.5">
              <label
                htmlFor="input-gateway-base-url"
                className="text-xs font-medium text-[var(--text-2)]"
              >
                Gateway Base URL（留空则使用同源地址）
              </label>
              <input
                id="input-gateway-base-url"
                className="sf-input font-mono text-xs"
                placeholder="例如 http://127.0.0.1:8787"
                value={draftBaseUrl}
                onChange={(e) => setDraftBaseUrl(e.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <label
                htmlFor="input-gateway-api-key"
                className="text-xs font-medium text-[var(--text-2)]"
              >
                API Key（若启动网关时指定了 --api-key）
              </label>
              <input
                id="input-gateway-api-key"
                type="password"
                className="sf-input font-mono text-xs"
                placeholder="可选 Bearer / x-api-key"
                value={draftApiKey}
                onChange={(e) => setDraftApiKey(e.target.value)}
              />
            </div>
            <div className="flex justify-end gap-2 pt-2">
              <button
                type="button"
                className="sf-btn"
                onClick={() => setSettingsOpen(false)}
              >
                取消
              </button>
              <button
                id="btn-save-connection"
                type="button"
                className="sf-btn sf-btn--primary"
                onClick={() => {
                  updateConnectionConfig(draftBaseUrl, draftApiKey);
                  setSettingsOpen(false);
                }}
              >
                保存并重连
              </button>
            </div>
          </div>
        </DialogContent>
      </Dialog>

      {/* =================================================================
         DIALOG 4: LIVE PROTOCOL PROBE
         ================================================================= */}
      <Dialog open={probeOpen} onOpenChange={setProbeOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>在线协议拨测（Live Probe）</DialogTitle>
            <DialogDescription>
              向当前网关发送真实推理请求，实时验证 ACP 思考流、会话复用与缓存命中统计。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3.5">
            <div className="sf-seg">
              <button
                type="button"
                className={`sf-seg__btn${probeProtocol === "openai_responses" ? " is-active" : ""}`}
                onClick={() => setProbeProtocol("openai_responses")}
              >
                OpenAI /v1/responses
              </button>
              <button
                type="button"
                className={`sf-seg__btn${probeProtocol === "anthropic_messages" ? " is-active" : ""}`}
                onClick={() => setProbeProtocol("anthropic_messages")}
              >
                Anthropic /v1/messages
              </button>
            </div>

            <div className="space-y-1">
              <label
                htmlFor="input-probe-model"
                className="text-xs font-medium text-[var(--text-2)]"
              >
                目标模型 ID
              </label>
              <input
                id="input-probe-model"
                className="sf-input font-mono text-xs"
                value={probeModel}
                onChange={(e) => setProbeModel(e.target.value)}
              />
            </div>

            <div className="space-y-1">
              <label
                htmlFor="input-probe-prompt"
                className="text-xs font-medium text-[var(--text-2)]"
              >
                测试提示词
              </label>
              <input
                id="input-probe-prompt"
                className="sf-input text-xs"
                value={probePrompt}
                onChange={(e) => setProbePrompt(e.target.value)}
              />
            </div>

            {probeResult && (
              <pre className="sf-code max-h-48">{probeResult}</pre>
            )}

            <div className="flex justify-end gap-2 pt-1">
              <button
                type="button"
                className="sf-btn"
                onClick={() => setProbeOpen(false)}
              >
                关闭
              </button>
              <button
                id="btn-send-probe"
                type="button"
                disabled={probeRunning}
                className="sf-btn sf-btn--primary"
                onClick={() => {
                  void runLiveProbe();
                }}
              >
                {probeRunning ? (
                  <>
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" /> 请求执行中…
                  </>
                ) : (
                  <>
                    <Play className="h-3.5 w-3.5" /> 发送实测请求
                  </>
                )}
              </button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
