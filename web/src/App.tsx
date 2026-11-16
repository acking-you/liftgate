import * as React from "react";
import { useMemo, useState } from "react";
import {
  Activity,
  AlertCircle,
  Boxes,
  CheckCircle2,
  Clock,
  Cpu,
  Database,
  Globe,
  Layers,
  Play,
  RefreshCw,
  Search,
  Server,
  Settings2,
  ShieldCheck,
  Sparkles,
  Terminal,
  Trash2,
  Wrench,
  Zap,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useGatewayTelemetry } from "@/hooks/useGatewayTelemetry";
import type { TaskPhase, TaskProtocol, TaskSnapshot } from "@/types/telemetry";

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
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

function formatTimestamp(ms: number): string {
  if (ms <= 0) return "-";
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

function formatNumber(n: number): string {
  return n.toLocaleString("en-US");
}

function renderPhaseBadge(phase: TaskPhase): React.JSX.Element {
  switch (phase) {
    case "initializing_session":
      return (
        <Badge variant="info" className="animate-pulse">
          <Clock className="h-3 w-3" /> 初始化会话
        </Badge>
      );
    case "thinking":
      return (
        <Badge variant="warning" className="animate-pulse">
          <Sparkles className="h-3 w-3" /> 深度思考中
        </Badge>
      );
    case "searching_web":
      return (
        <Badge variant="info" className="animate-pulse">
          <Globe className="h-3 w-3" /> 联网搜索中
        </Badge>
      );
    case "fetching_web":
      return (
        <Badge variant="info" className="animate-pulse">
          <Globe className="h-3 w-3" /> 抓取网页中
        </Badge>
      );
    case "streaming_text":
      return (
        <Badge variant="info" className="animate-pulse">
          <Activity className="h-3 w-3" /> 流式生成中
        </Badge>
      );
    case "calling_client_tool":
      return (
        <Badge variant="warning">
          <Wrench className="h-3 w-3" /> 调用客户端工具
        </Badge>
      );
    case "completed":
      return (
        <Badge variant="success">
          <CheckCircle2 className="h-3 w-3" /> 已完成
        </Badge>
      );
    case "failed":
      return (
        <Badge variant="destructive">
          <AlertCircle className="h-3 w-3" /> 失败
        </Badge>
      );
    case "cancelled":
      return <Badge variant="secondary">已取消 / 断开</Badge>;
  }
}

function renderProtocolBadge(protocol: TaskProtocol): React.JSX.Element {
  if (protocol === "openai_responses") {
    return <Badge variant="outline">OpenAI /v1/responses</Badge>;
  }
  return <Badge variant="outline">Anthropic /v1/messages</Badge>;
}

export function App(): React.JSX.Element {
  const {
    snapshot,
    connectionState,
    lastError,
    baseUrl,
    apiKey,
    updateConnectionConfig,
    refreshSnapshot,
    closeIdleSession,
  } = useGatewayTelemetry();

  const [protocolFilter, setProtocolFilter] = useState<"all" | TaskProtocol>("all");
  const [modelQuery, setModelQuery] = useState("");
  const [selectedTask, setSelectedTask] = useState<TaskSnapshot | null>(null);

  // Connection config modal state
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [draftBaseUrl, setDraftBaseUrl] = useState(baseUrl);
  const [draftApiKey, setDraftApiKey] = useState(apiKey);

  // Live probe modal state
  const [probeOpen, setProbeOpen] = useState(false);
  const [probeProtocol, setProbeProtocol] = useState<TaskProtocol>("openai_responses");
  const [probeModel, setProbeModel] = useState("gemini-4-hydrogen-low");
  const [probePrompt, setProbePrompt] = useState(
    "请用一句话介绍 Rust 2024 Edition 的优势。",
  );
  const [probeRunning, setProbeRunning] = useState(false);
  const [probeResult, setProbeResult] = useState<string | null>(null);

  const acp = snapshot?.acp_process;
  const pool = snapshot?.session_pool;
  const metrics = snapshot?.metrics;
  const activeTasks = snapshot?.active_tasks ?? [];
  const recentTasks = snapshot?.recent_tasks ?? [];
  const models = snapshot?.available_models ?? [];

  const filteredRecentTasks = useMemo(() => {
    if (protocolFilter === "all") return recentTasks;
    return recentTasks.filter((t) => t.protocol === protocolFilter);
  }, [recentTasks, protocolFilter]);

  const filteredModels = useMemo(() => {
    const q = modelQuery.trim().toLowerCase();
    if (!q) return models;
    return models.filter(
      (m) =>
        m.modelId.toLowerCase().includes(q) || m.name.toLowerCase().includes(q),
    );
  }, [models, modelQuery]);

  const sessionHitRate = useMemo(() => {
    if (!pool) return 0;
    const total = pool.total_sessions_created + pool.total_sessions_reused;
    if (total === 0) return 0;
    return Math.round((pool.total_sessions_reused / total) * 100);
  }, [pool]);

  const idlePoolPercent = useMemo(() => {
    if (!pool || pool.max_idle_sessions === 0) return 0;
    return Math.round((pool.idle_cached_sessions / pool.max_idle_sessions) * 100);
  }, [pool]);

  const effectiveGatewayOrigin = useMemo(() => {
    if (baseUrl.trim().length > 0) return baseUrl.trim().replace(/\/+$/, "");
    if (typeof window !== "undefined" && window.location.port !== "5173") {
      return window.location.origin;
    }
    return "http://127.0.0.1:8787";
  }, [baseUrl]);

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
          data.output
            ?.find((o) => o.type === "message")
            ?.content?.[0]?.text ?? JSON.stringify(data);
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

  return (
    <div className="min-h-screen bg-background text-foreground">
      {/* Top Header */}
      <header className="sticky top-0 z-30 border-b bg-background/90 backdrop-blur-md">
        <div className="mx-auto flex max-w-7xl flex-wrap items-center justify-between gap-3 px-6 py-3.5">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-primary text-primary-foreground shadow-xs">
              <Zap className="h-5 w-5" />
            </div>
            <div>
              <div className="flex items-center gap-2">
                <h1 className="text-base font-semibold tracking-tight">
                  Liftgate
                </h1>
                <Badge variant="secondary">
                  v{snapshot?.gateway_version ?? "0.1.0"}
                </Badge>
              </div>
              <p className="text-xs text-muted-foreground">
                OpenAI Responses &amp; Anthropic Messages 跨平台协议网关实时监控台
              </p>
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            {connectionState === "live" ? (
              <Badge variant="success">
                <span className="h-2 w-2 rounded-full bg-emerald-400 animate-ping" />
                SSE 实时流已连接
              </Badge>
            ) : (
              <Badge variant="warning">
                <RefreshCw className="h-3 w-3 animate-spin" />
                {connectionState === "connecting" ? "正在连接网关..." : "SSE 重连中"}
              </Badge>
            )}

            {acp && (
              <Badge variant={acp.connected ? "info" : "destructive"}>
                <Server className="h-3 w-3" />
                {acp.connected
                  ? `ACP 在线${acp.pid !== null ? ` · PID ${acp.pid}` : ""}`
                  : "ACP 管道断开"}
              </Badge>
            )}

            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                setProbeOpen(true);
              }}
            >
              <Play className="h-3.5 w-3.5" />
              在线拨测
            </Button>

            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                setDraftBaseUrl(baseUrl);
                setDraftApiKey(apiKey);
                setSettingsOpen(true);
              }}
            >
              <Settings2 className="h-3.5 w-3.5" />
              连接设置
            </Button>

            <Button
              variant="ghost"
              size="icon"
              title="立即刷新快照"
              onClick={() => {
                void refreshSnapshot();
              }}
            >
              <RefreshCw className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </header>

      <main className="mx-auto max-w-7xl space-y-6 px-6 py-6">
        {lastError && (
          <div className="flex items-center gap-2 rounded-lg border border-red-500/40 bg-red-500/10 px-4 py-3 text-sm text-red-300">
            <AlertCircle className="h-4 w-4 shrink-0" />
            <span>
              无法连接网关遥测接口（{lastError}）。请确认{" "}
              <code className="rounded bg-black/30 px-1.5 py-0.5 font-mono text-xs">
                liftgate
              </code>{" "}
              已启动，或在右上角「连接设置」检查地址与密钥。
            </span>
          </div>
        )}

        {/* ACP Master Process Supervision Overview Card */}
        <Card>
          <CardHeader className="pb-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <CardTitle className="flex items-center gap-2 text-base">
                  <Cpu className="h-4 w-4 text-sky-400" />
                  ACP 主进程与跨平台监管状态（agy acp --process-per-session）
                </CardTitle>
                <CardDescription className="mt-1">
                  通过官方标准 JSON-RPC 2.0 over stdio 监管{" "}
                  {acp?.agent_name ?? "Antigravity ACP Server"}{" "}
                  {acp?.agent_version ? `v${acp.agent_version}` : ""}
                </CardDescription>
              </div>
              {acp && (
                <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                  <Badge variant="outline">
                    {acp.os} / {acp.arch}
                  </Badge>
                  <Badge variant="outline">ACP Protocol v{acp.protocol_version}</Badge>
                  <Badge variant="secondary">
                    运行时长 {formatUptime(acp.uptime_secs)}
                  </Badge>
                </div>
              )}
            </div>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
              <div className="rounded-lg border bg-muted/30 p-3">
                <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <ShieldCheck className="h-3.5 w-3.5 text-emerald-400" />
                  进程树隔离策略
                </div>
                <div className="mt-1 text-xs font-medium leading-relaxed">
                  {acp?.supervision_strategy ?? "-"}
                </div>
              </div>

              <div className="rounded-lg border bg-muted/30 p-3">
                <div className="text-xs text-muted-foreground">默认路由模型 / 模式</div>
                <div className="mt-1 flex items-center gap-2 text-xs font-mono font-semibold">
                  <span>{acp?.default_model ?? "-"}</span>
                  {acp && <Badge variant="secondary">{acp.acp_mode}</Badge>}
                </div>
              </div>

              <div className="rounded-lg border bg-muted/30 p-3">
                <div className="text-xs text-muted-foreground">
                  JSON-RPC 2.0 帧收发统计
                </div>
                <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs font-mono">
                  <span>请求: {formatNumber(acp?.rpc_requests_sent ?? 0)}</span>
                  <span>响应: {formatNumber(acp?.rpc_responses_received ?? 0)}</span>
                  <span>流通知: {formatNumber(acp?.rpc_notifications_received ?? 0)}</span>
                </div>
              </div>

              <div className="rounded-lg border bg-muted/30 p-3">
                <div className="text-xs text-muted-foreground">
                  当前挂起 RPC / 活跃流通道
                </div>
                <div className="mt-1 flex items-center gap-4 text-xs font-mono">
                  <span>
                    Pending RPC:{" "}
                    <strong className="text-foreground">
                      {acp?.rpc_pending_in_flight ?? 0}
                    </strong>
                  </span>
                  <span>
                    Prompt Streams:{" "}
                    <strong className="text-sky-400">
                      {acp?.active_prompt_streams ?? 0}
                    </strong>
                  </span>
                </div>
              </div>
            </div>

            {acp && (
              <div className="grid grid-cols-1 gap-2 rounded-lg border bg-muted/20 px-3.5 py-2.5 text-xs md:grid-cols-2">
                <div className="truncate">
                  <span className="text-muted-foreground">可执行文件: </span>
                  <code className="font-mono text-foreground/90">{acp.binary_path}</code>
                </div>
                <div className="truncate">
                  <span className="text-muted-foreground">隔离会话目录 (CWD): </span>
                  <code className="font-mono text-foreground/90">{acp.session_cwd}</code>
                </div>
                <div className="truncate">
                  <span className="text-muted-foreground">AGY 代理策略: </span>
                  <code className="font-mono text-foreground/90">
                    {acp.agy_proxy ? `强制代理 (${acp.agy_proxy})` : "直连 (默认无代理)"}
                  </code>
                </div>
                <div className="truncate">
                  <span className="text-muted-foreground">AGY 空闲自动更新: </span>
                  <code className="font-mono text-foreground/90">
                    {(acp.auto_update_enabled ?? true) ? "开启" : "关闭"}
                    {acp.latest_known_version ? ` · 上游 v${acp.latest_known_version}` : ""}
                    {acp.auto_update_note ? ` (${acp.auto_update_note})` : ""}
                  </code>
                </div>
              </div>
            )}
          </CardContent>
        </Card>

        {/* 4 KPI Summary Cards */}
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Card>
            <CardHeader className="pb-2">
              <CardDescription className="flex items-center justify-between">
                <span>当前运行中任务</span>
                <Activity className="h-4 w-4 text-sky-400" />
              </CardDescription>
              <CardTitle className="text-3xl font-bold tabular-nums">
                {activeTasks.length}
              </CardTitle>
            </CardHeader>
            <CardContent className="text-xs text-muted-foreground">
              活跃工作子进程: <strong>{pool?.active_leased_sessions ?? 0}</strong> 个 ·
              流式实时追踪
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardDescription className="flex items-center justify-between">
                <span>多轮会话池复用率</span>
                <Layers className="h-4 w-4 text-emerald-400" />
              </CardDescription>
              <CardTitle className="text-3xl font-bold tabular-nums">
                {sessionHitRate}%
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-2 text-xs text-muted-foreground">
              <Progress value={idlePoolPercent} />
              <div className="flex justify-between">
                <span>
                  空闲缓存: {pool?.idle_cached_sessions ?? 0} /{" "}
                  {pool?.max_idle_sessions ?? 0}
                </span>
                <span>
                  复用 {pool?.total_sessions_reused ?? 0} · 新建{" "}
                  {pool?.total_sessions_created ?? 0}
                </span>
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardDescription className="flex items-center justify-between">
                <span>累计 API 请求吞吐</span>
                <Boxes className="h-4 w-4 text-violet-400" />
              </CardDescription>
              <CardTitle className="text-3xl font-bold tabular-nums">
                {formatNumber(metrics?.total_requests ?? 0)}
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-1 text-xs text-muted-foreground">
              <div className="flex justify-between">
                <span className="text-emerald-400">
                  成功 {metrics?.completed_requests ?? 0}
                </span>
                <span className="text-red-400">失败 {metrics?.failed_requests ?? 0}</span>
                <span>取消 {metrics?.cancelled_requests ?? 0}</span>
              </div>
              <div className="flex justify-between pt-0.5">
                <span>OpenAI: {metrics?.openai_requests ?? 0}</span>
                <span>Anthropic: {metrics?.anthropic_requests ?? 0}</span>
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardDescription className="flex items-center justify-between">
                <span>累计 Token 统计</span>
                <Database className="h-4 w-4 text-amber-400" />
              </CardDescription>
              <CardTitle className="text-3xl font-bold tabular-nums">
                {formatNumber(
                  (metrics?.total_input_tokens ?? 0) +
                    (metrics?.total_output_tokens ?? 0),
                )}
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-1 text-xs text-muted-foreground">
              <div className="flex justify-between">
                <span>输入: {formatNumber(metrics?.total_input_tokens ?? 0)}</span>
                <span>输出: {formatNumber(metrics?.total_output_tokens ?? 0)}</span>
              </div>
              <div className="flex justify-between">
                <span>思考: {formatNumber(metrics?.total_thought_tokens ?? 0)}</span>
                <span>缓存命中: {formatNumber(metrics?.total_cached_tokens ?? 0)}</span>
              </div>
            </CardContent>
          </Card>
        </div>

        {/* Main Workspace Tabs */}
        <Tabs defaultValue="tasks">
          <TabsList className="flex flex-wrap h-auto gap-1">
            <TabsTrigger value="tasks">
              <Activity className="h-4 w-4" />
              任务实时态势 ({activeTasks.length} 运行中 / {recentTasks.length} 历史)
            </TabsTrigger>
            <TabsTrigger value="sessions">
              <Layers className="h-4 w-4" />
              ACP 会话池 ({pool?.sessions.length ?? 0})
            </TabsTrigger>
            <TabsTrigger value="models">
              <Sparkles className="h-4 w-4" />
              可用模型目录 ({models.length})
            </TabsTrigger>
            <TabsTrigger value="logs">
              <Terminal className="h-4 w-4" />
              进程日志与快速接入
            </TabsTrigger>
          </TabsList>

          {/* TAB 1: Active & Historical Tasks */}
          <TabsContent value="tasks" className="space-y-5">
            {/* Active In-Flight Tasks */}
            <Card>
              <CardHeader className="pb-3">
                <CardTitle className="text-base">
                  当前执行中的任务（In-Flight Tasks）
                </CardTitle>
                <CardDescription>
                  实时展示正在执行推理、思考、联网搜索或工具调用的请求流状态
                </CardDescription>
              </CardHeader>
              <CardContent>
                {activeTasks.length === 0 ? (
                  <div className="flex flex-col items-center justify-center rounded-lg border border-dashed py-8 text-center text-sm text-muted-foreground">
                    <CheckCircle2 className="mb-2 h-6 w-6 text-muted-foreground/60" />
                    当前暂无执行中的请求，点击右上角「在线拨测」或通过 Codex / Claude Code
                    发起调用即可实时观测。
                  </div>
                ) : (
                  <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
                    {activeTasks.map((task) => (
                      <div
                        key={task.task_id}
                        className="space-y-2.5 rounded-lg border bg-muted/25 p-4"
                      >
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <div className="flex items-center gap-2">
                            {renderPhaseBadge(task.phase)}
                            {renderProtocolBadge(task.protocol)}
                          </div>
                          <span className="font-mono text-xs text-muted-foreground">
                            已耗时 {formatDurationMs(task.elapsed_ms)}
                          </span>
                        </div>

                        <div className="flex flex-wrap items-center gap-2 text-xs">
                          <code className="rounded bg-secondary px-1.5 py-0.5 font-mono">
                            {task.task_id}
                          </code>
                          <span className="font-mono text-muted-foreground">
                            {task.requested_model === task.resolved_model
                              ? task.resolved_model
                              : `${task.requested_model} → ${task.resolved_model}`}
                          </span>
                          {task.session_id && (
                            <Badge variant={task.session_reused ? "success" : "secondary"}>
                              {task.session_reused ? "复用会话" : "新建会话"} ·{" "}
                              {task.session_id.slice(0, 12)}
                            </Badge>
                          )}
                        </div>

                        {task.prompt_preview && (
                          <p className="line-clamp-2 rounded bg-background/60 px-2.5 py-1.5 text-xs text-muted-foreground">
                            <strong className="text-foreground/80">Prompt: </strong>
                            {task.prompt_preview}
                          </p>
                        )}

                        {task.output_preview && (
                          <p className="line-clamp-2 rounded bg-sky-500/5 border border-sky-500/20 px-2.5 py-1.5 text-xs">
                            <strong className="text-sky-400">实时输出: </strong>
                            {task.output_preview}
                          </p>
                        )}

                        <div className="flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
                          <span>思考字符: {task.thought_chars}</span>
                          <span>输出字符: {task.output_chars}</span>
                          {task.native_web_searches > 0 && (
                            <span className="text-sky-400">
                              原生 Web 搜索: {task.native_web_searches} 次
                            </span>
                          )}
                          {(task.native_web_fetches ?? 0) > 0 && (
                            <span className="text-emerald-400">
                              原生网页抓取: {task.native_web_fetches} 次
                            </span>
                          )}
                          {task.client_tool_calls.length > 0 && (
                            <span className="text-amber-400">
                              工具调用: {task.client_tool_calls.join(", ")}
                            </span>
                          )}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>

            {/* Recent Task History Table */}
            <Card>
              <CardHeader className="pb-3">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <CardTitle className="text-base">
                      最近完成的任务记录（最多保留最近 100 条）
                    </CardTitle>
                    <CardDescription>
                      点击任意任务行可查看完整输入输出摘要、工具调用链与 Token 明细
                    </CardDescription>
                  </div>
                  <div className="flex items-center gap-1.5">
                    <Button
                      variant={protocolFilter === "all" ? "default" : "outline"}
                      size="sm"
                      onClick={() => setProtocolFilter("all")}
                    >
                      全部
                    </Button>
                    <Button
                      variant={
                        protocolFilter === "openai_responses" ? "default" : "outline"
                      }
                      size="sm"
                      onClick={() => setProtocolFilter("openai_responses")}
                    >
                      OpenAI Responses
                    </Button>
                    <Button
                      variant={
                        protocolFilter === "anthropic_messages" ? "default" : "outline"
                      }
                      size="sm"
                      onClick={() => setProtocolFilter("anthropic_messages")}
                    >
                      Anthropic Messages
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                {filteredRecentTasks.length === 0 ? (
                  <div className="py-8 text-center text-sm text-muted-foreground">
                    暂无历史任务记录
                  </div>
                ) : (
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead>时间</TableHead>
                        <TableHead>任务 ID / 协议</TableHead>
                        <TableHead>模型路由</TableHead>
                        <TableHead>ACP 会话</TableHead>
                        <TableHead>状态与工具</TableHead>
                        <TableHead className="text-right">耗时</TableHead>
                        <TableHead className="text-right">Tokens (In/Out)</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {filteredRecentTasks.map((task) => (
                        <TableRow
                          key={task.task_id}
                          className="cursor-pointer"
                          onClick={() => setSelectedTask(task)}
                        >
                          <TableCell className="font-mono text-xs text-muted-foreground">
                            {formatTimestamp(task.started_at_unix_ms)}
                          </TableCell>
                          <TableCell>
                            <div className="font-mono text-xs font-medium">
                              {task.task_id}
                            </div>
                            <div className="mt-0.5 flex items-center gap-1 text-[11px] text-muted-foreground">
                              <span>
                                {task.protocol === "openai_responses"
                                  ? "/v1/responses"
                                  : "/v1/messages"}
                              </span>
                              <span>·</span>
                              <span>{task.stream ? "SSE" : "JSON"}</span>
                            </div>
                          </TableCell>
                          <TableCell className="font-mono text-xs">
                            {task.requested_model === task.resolved_model ? (
                              task.resolved_model
                            ) : (
                              <div>
                                <div className="text-muted-foreground">
                                  {task.requested_model}
                                </div>
                                <div className="text-foreground">
                                  → {task.resolved_model}
                                </div>
                              </div>
                            )}
                          </TableCell>
                          <TableCell className="text-xs">
                            {task.session_id ? (
                              <div className="flex items-center gap-1.5">
                                <code className="font-mono">
                                  {task.session_id.slice(0, 10)}…
                                </code>
                                {task.session_reused && (
                                  <Badge variant="success" className="px-1.5 py-0">
                                    复用
                                  </Badge>
                                )}
                              </div>
                            ) : (
                              "-"
                            )}
                          </TableCell>
                          <TableCell>
                            <div className="flex flex-wrap items-center gap-1.5">
                              {renderPhaseBadge(task.phase)}
                              {task.client_tool_calls.length > 0 && (
                                <Badge variant="warning">
                                  {task.client_tool_calls.length} 工具
                                </Badge>
                              )}
                              {task.native_web_searches > 0 && (
                                <Badge variant="info">
                                  Web×{task.native_web_searches}
                                </Badge>
                              )}
                              {(task.native_web_fetches ?? 0) > 0 && (
                                <Badge variant="info">
                                  Fetch×{task.native_web_fetches}
                                </Badge>
                              )}
                            </div>
                          </TableCell>
                          <TableCell className="text-right font-mono text-xs">
                            {formatDurationMs(task.elapsed_ms)}
                          </TableCell>
                          <TableCell className="text-right font-mono text-xs">
                            {task.usage
                              ? `${formatNumber(task.usage.inputTokens)} / ${formatNumber(task.usage.outputTokens)}`
                              : "-"}
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                )}
              </CardContent>
            </Card>
          </TabsContent>

          {/* TAB 2: ACP Session Pool */}
          <TabsContent value="sessions">
            <Card>
              <CardHeader className="pb-3">
                <CardTitle className="text-base">
                  ACP 会话池与独立工作子进程（--process-per-session）
                </CardTitle>
                <CardDescription>
                  每个 ACP 会话对应一个独立的子进程上下文；空闲会话通过 LRU 缓存供{" "}
                  <code>previous_response_id</code> 或前缀哈希无状态续接复用（已累计淘汰/释放{" "}
                  {pool?.total_sessions_closed_or_evicted ?? 0} 个会话）
                </CardDescription>
              </CardHeader>
              <CardContent>
                {!pool || pool.sessions.length === 0 ? (
                  <div className="py-10 text-center text-sm text-muted-foreground">
                    当前会话池为空，首次请求到达时将自动按需创建 ACP 会话。
                  </div>
                ) : (
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead>Session ID</TableHead>
                        <TableHead>状态</TableHead>
                        <TableHead>绑定模型</TableHead>
                        <TableHead>已执行轮次</TableHead>
                        <TableHead>累计消息数</TableHead>
                        <TableHead>最近 Response ID</TableHead>
                        <TableHead>最近活跃</TableHead>
                        <TableHead className="text-right">操作</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {pool.sessions.map((sess) => (
                        <TableRow key={sess.session_id}>
                          <TableCell className="font-mono text-xs font-medium">
                            {sess.session_id}
                          </TableCell>
                          <TableCell>
                            {sess.state === "running" ? (
                              <Badge variant="info" className="animate-pulse">
                                执行中 (Leased)
                              </Badge>
                            ) : (
                              <Badge variant="secondary">空闲缓存 (Idle)</Badge>
                            )}
                          </TableCell>
                          <TableCell className="font-mono text-xs">
                            {sess.model_id}
                          </TableCell>
                          <TableCell className="font-mono text-xs">
                            {sess.turn_count}
                          </TableCell>
                          <TableCell className="font-mono text-xs">
                            {sess.message_count}
                          </TableCell>
                          <TableCell className="font-mono text-xs text-muted-foreground">
                            {sess.last_response_id ?? "-"}
                          </TableCell>
                          <TableCell className="font-mono text-xs text-muted-foreground">
                            {formatTimestamp(sess.last_active_unix_ms)}
                          </TableCell>
                          <TableCell className="text-right">
                            {sess.state === "idle" && (
                              <Button
                                variant="ghost"
                                size="sm"
                                className="text-red-400 hover:text-red-300"
                                onClick={() => {
                                  void closeIdleSession(sess.session_id);
                                }}
                              >
                                <Trash2 className="h-3.5 w-3.5" />
                                关闭释放
                              </Button>
                            )}
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                )}
              </CardContent>
            </Card>
          </TabsContent>

          {/* TAB 3: Available Models */}
          <TabsContent value="models">
            <Card>
              <CardHeader className="pb-3">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <CardTitle className="text-base">
                      当前账号可用 Antigravity 模型目录（共 {models.length} 个）
                    </CardTitle>
                    <CardDescription>
                      启动时自动从 <code>agy acp</code> 会话握手探测，支持精确模型名与
                      Codex / Claude Code 别名自动映射
                    </CardDescription>
                  </div>
                  <div className="relative w-64">
                    <Search className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
                    <Input
                      placeholder="搜索模型 ID 或名称..."
                      className="pl-8"
                      value={modelQuery}
                      onChange={(e) => setModelQuery(e.target.value)}
                    />
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
                  {filteredModels.map((m) => (
                    <div
                      key={m.modelId}
                      className="flex items-center justify-between gap-2 rounded-lg border bg-muted/20 p-3"
                    >
                      <div className="min-w-0">
                        <div className="truncate font-mono text-xs font-semibold">
                          {m.modelId}
                        </div>
                        <div className="truncate text-xs text-muted-foreground">
                          {m.name || m.modelId}
                        </div>
                      </div>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => {
                          setProbeModel(m.modelId);
                          setProbeOpen(true);
                        }}
                      >
                        实测
                      </Button>
                    </div>
                  ))}
                </div>
              </CardContent>
            </Card>
          </TabsContent>

          {/* TAB 4: Stderr Logs & Integration Snippets */}
          <TabsContent value="logs" className="space-y-4">
            <Card>
              <CardHeader className="pb-3">
                <CardTitle className="text-base">
                  ACP 子进程诊断日志（stderr 最近 100 行）
                </CardTitle>
                <CardDescription>
                  用于排查 <code>agy acp</code> 底层认证、网络或子进程异常输出
                </CardDescription>
              </CardHeader>
              <CardContent>
                {!acp || acp.recent_stderr.length === 0 ? (
                  <div className="rounded-lg border bg-black/40 p-4 font-mono text-xs text-emerald-400">
                    [healthy] agy acp stderr 无异常告警输出
                  </div>
                ) : (
                  <div className="max-h-72 overflow-y-auto rounded-lg border bg-black/60 p-3 font-mono text-xs space-y-1">
                    {acp.recent_stderr.map((entry, idx) => (
                      <div key={`${entry.timestamp_unix_ms}-${idx}`} className="leading-relaxed">
                        <span className="text-muted-foreground mr-2">
                          [{formatTimestamp(entry.timestamp_unix_ms)}]
                        </span>
                        <span className="text-amber-200">{entry.line}</span>
                      </div>
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>

            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <Card>
                <CardHeader className="pb-2">
                  <CardTitle className="text-sm">
                    OpenAI Codex CLI 接入配置（~/.codex/config.toml）
                  </CardTitle>
                </CardHeader>
                <CardContent>
                  <pre className="overflow-x-auto rounded-lg border bg-black/50 p-3 font-mono text-xs leading-relaxed">
                    {`# 推荐一键启动（自动从 /v1/codex-models 同步全量模型目录，不改动原有配置）
.\\start-agy.cmd -Codex   # Linux/macOS: ./start-agy.sh --codex

# 或手动写入 ~/.codex/config.toml
model = "${acp?.default_model ?? "gemini-4-carbon-high"}"
model_provider = "agy"
model_catalog_json = ".agy-runtime/codex-model-catalog.json"
web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "${effectiveGatewayOrigin}/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"`}
                  </pre>
                </CardContent>
              </Card>

              <Card>
                <CardHeader className="pb-2">
                  <CardTitle className="text-sm">
                    Claude Code 接入环境变量（Windows / Linux / macOS）
                  </CardTitle>
                </CardHeader>
                <CardContent>
                  <pre className="overflow-x-auto rounded-lg border bg-black/50 p-3 font-mono text-xs leading-relaxed">
                    {`# Linux / macOS
export ANTHROPIC_BASE_URL="${effectiveGatewayOrigin}"
export ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
export ANTHROPIC_MODEL="claude-sonnet-5-5-high"
claude

# Windows PowerShell
$env:ANTHROPIC_BASE_URL="${effectiveGatewayOrigin}"
$env:ANTHROPIC_AUTH_TOKEN="${apiKey || "local-agy"}"
$env:ANTHROPIC_MODEL="claude-sonnet-5-5-high"
claude`}
                  </pre>
                </CardContent>
              </Card>
            </div>
          </TabsContent>
        </Tabs>
      </main>

      {/* Task Detail Inspection Dialog */}
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
                <DialogTitle className="flex items-center gap-2 font-mono text-base">
                  <span>{selectedTask.task_id}</span>
                  {renderPhaseBadge(selectedTask.phase)}
                </DialogTitle>
                <DialogDescription>
                  {selectedTask.protocol === "openai_responses"
                    ? "OpenAI Responses API (/v1/responses)"
                    : "Anthropic Messages API (/v1/messages)"}{" "}
                  · 耗时 {formatDurationMs(selectedTask.elapsed_ms)}
                </DialogDescription>
              </DialogHeader>

              <Separator />

              <div className="grid grid-cols-2 gap-3 text-xs">
                <div>
                  <span className="text-muted-foreground">请求模型: </span>
                  <code className="font-mono">{selectedTask.requested_model}</code>
                </div>
                <div>
                  <span className="text-muted-foreground">实际路由模型: </span>
                  <code className="font-mono">{selectedTask.resolved_model}</code>
                </div>
                <div>
                  <span className="text-muted-foreground">绑定 ACP 会话: </span>
                  <code className="font-mono">{selectedTask.session_id ?? "-"}</code>
                </div>
                <div>
                  <span className="text-muted-foreground">停止原因 (stop_reason): </span>
                  <code className="font-mono">{selectedTask.stop_reason ?? "-"}</code>
                </div>
              </div>

              {selectedTask.usage && (
                <div className="rounded-lg border bg-muted/30 p-3 text-xs">
                  <div className="font-medium mb-1.5">Token 明细（ACP Usage）</div>
                  <div className="grid grid-cols-3 gap-2 font-mono">
                    <span>输入: {formatNumber(selectedTask.usage.inputTokens)}</span>
                    <span>输出: {formatNumber(selectedTask.usage.outputTokens)}</span>
                    <span>思考: {formatNumber(selectedTask.usage.thoughtTokens)}</span>
                    <span>
                      缓存读: {formatNumber(selectedTask.usage.cachedReadTokens)}
                    </span>
                    <span>
                      缓存写: {formatNumber(selectedTask.usage.cachedWriteTokens)}
                    </span>
                    <span>总计: {formatNumber(selectedTask.usage.totalTokens)}</span>
                  </div>
                </div>
              )}

              {(selectedTask.native_web_searches > 0 ||
                (selectedTask.native_web_fetches ?? 0) > 0) && (
                <div className="flex flex-wrap gap-4 text-xs">
                  {selectedTask.native_web_searches > 0 && (
                    <span>
                      <span className="text-muted-foreground">原生 Web 搜索: </span>
                      <span className="font-mono text-sky-400">
                        {selectedTask.native_web_searches} 次
                      </span>
                    </span>
                  )}
                  {(selectedTask.native_web_fetches ?? 0) > 0 && (
                    <span>
                      <span className="text-muted-foreground">原生网页抓取: </span>
                      <span className="font-mono text-emerald-400">
                        {selectedTask.native_web_fetches} 次
                      </span>
                    </span>
                  )}
                </div>
              )}

              {selectedTask.client_tool_calls.length > 0 && (
                <div className="text-xs">
                  <span className="text-muted-foreground">触发客户端工具: </span>
                  <span className="font-mono text-amber-400">
                    {selectedTask.client_tool_calls.join(", ")}
                  </span>
                </div>
              )}

              {selectedTask.prompt_preview && (
                <div className="space-y-1">
                  <div className="text-xs font-medium text-muted-foreground">
                    输入摘要
                  </div>
                  <pre className="whitespace-pre-wrap break-words rounded-lg border bg-muted/20 p-3 font-mono text-xs">
                    {selectedTask.prompt_preview}
                  </pre>
                </div>
              )}

              {selectedTask.output_preview && (
                <div className="space-y-1">
                  <div className="text-xs font-medium text-muted-foreground">
                    输出摘要
                  </div>
                  <pre className="whitespace-pre-wrap break-words rounded-lg border bg-muted/20 p-3 font-mono text-xs">
                    {selectedTask.output_preview}
                  </pre>
                </div>
              )}

              {selectedTask.error_message && (
                <div className="rounded-lg border border-red-500/40 bg-red-500/10 p-3 font-mono text-xs text-red-300">
                  {selectedTask.error_message}
                </div>
              )}
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Gateway Endpoint Settings Dialog */}
      <Dialog open={settingsOpen} onOpenChange={setSettingsOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>网关连接设置</DialogTitle>
            <DialogDescription>
              默认自动代理或连接同源网关；若网关运行在其他端口，可在此指定地址与可选密钥。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium">
                Gateway Base URL（留空则使用当前源 / Vite 代理）
              </label>
              <Input
                placeholder="例如 http://127.0.0.1:8787"
                value={draftBaseUrl}
                onChange={(e) => setDraftBaseUrl(e.target.value)}
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium">
                API Key（若启动网关时设置了 --api-key）
              </label>
              <Input
                type="password"
                placeholder="可选 Bearer / x-api-key"
                value={draftApiKey}
                onChange={(e) => setDraftApiKey(e.target.value)}
              />
            </div>
            <div className="flex justify-end gap-2 pt-2">
              <Button variant="outline" onClick={() => setSettingsOpen(false)}>
                取消
              </Button>
              <Button
                onClick={() => {
                  updateConnectionConfig(draftBaseUrl, draftApiKey);
                  setSettingsOpen(false);
                }}
              >
                保存并重连
              </Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>

      {/* Live API Probe Dialog */}
      <Dialog open={probeOpen} onOpenChange={setProbeOpen}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>在线协议拨测（Live Probe）</DialogTitle>
            <DialogDescription>
              直接向网关发送真实请求，可在主面板实时观察 ACP 子进程思考、流式输出与会话入池全过程
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="flex gap-2">
              <Button
                type="button"
                size="sm"
                variant={
                  probeProtocol === "openai_responses" ? "default" : "outline"
                }
                onClick={() => setProbeProtocol("openai_responses")}
              >
                OpenAI /v1/responses
              </Button>
              <Button
                type="button"
                size="sm"
                variant={
                  probeProtocol === "anthropic_messages" ? "default" : "outline"
                }
                onClick={() => setProbeProtocol("anthropic_messages")}
              >
                Anthropic /v1/messages
              </Button>
            </div>
            <div className="space-y-1">
              <label className="text-xs font-medium">目标模型 ID</label>
              <Input
                value={probeModel}
                onChange={(e) => setProbeModel(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <label className="text-xs font-medium">测试消息内容</label>
              <Input
                value={probePrompt}
                onChange={(e) => setProbePrompt(e.target.value)}
              />
            </div>
            {probeResult && (
              <pre className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words rounded-lg border bg-muted/30 p-3 font-mono text-xs">
                {probeResult}
              </pre>
            )}
            <div className="flex justify-end gap-2 pt-1">
              <Button variant="outline" onClick={() => setProbeOpen(false)}>
                关闭
              </Button>
              <Button
                disabled={probeRunning}
                onClick={() => {
                  void runLiveProbe();
                }}
              >
                {probeRunning ? (
                  <>
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" /> 请求执行中...
                  </>
                ) : (
                  <>
                    <Play className="h-3.5 w-3.5" /> 发送实测请求
                  </>
                )}
              </Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
