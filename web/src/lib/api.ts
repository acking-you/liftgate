import type {
  CacheTelemetrySummary,
  ContextCaptureConfigUpdate,
  ContextCaptureDetailResponse,
  ContextCaptureSummary,
  ContextCapturesResponse,
  GatewayTelemetrySnapshot,
  LowCacheHitsResponse,
  LowCachePayloadDetailResponse,
  ModelPrice,
  OptimizationMode,
  OptimizationPolicy,
  PricingResponse,
  SessionToolCatalog,
  TaskProtocol,
  ToolCallDetail,
  ToolCallsResponse,
  UsageResponse,
  UsageSeriesResponse,
  UsageSummary,
} from "@/types/telemetry";

export type StreamConnectionState = "connecting" | "live" | "reconnecting";

export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** True when the gateway simply does not implement the endpoint yet. */
export function isUnsupported(err: unknown): boolean {
  return err instanceof ApiError && (err.status === 404 || err.status === 405 || err.status === 501);
}

export function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export interface SnapshotHandlers {
  onSnapshot: (snapshot: GatewayTelemetrySnapshot) => void;
  onState: (state: StreamConnectionState) => void;
  onError: (message: string | null) => void;
}

export interface CacheConfigUpdate {
  max_low_cache_records?: number;
  low_cache_threshold_pct?: number;
  capture_request_details?: boolean;
}

/** Everything the dashboard asks of a gateway; the dev-only demo backend implements it too. */
export interface GatewayClient {
  /** True when the data is synthetic (dev demo mode). */
  readonly demo: boolean;
  /** Origin harnesses should be pointed at, used by the integration snippets. */
  readonly origin: string;

  subscribe(handlers: SnapshotHandlers): () => void;
  snapshot(): Promise<GatewayTelemetrySnapshot>;

  lowCacheHits(): Promise<LowCacheHitsResponse>;
  lowCacheHitDetail(taskId: string): Promise<LowCachePayloadDetailResponse>;
  clearLowCacheHits(resetTotals: boolean): Promise<CacheTelemetrySummary>;
  updateCacheConfig(update: CacheConfigUpdate): Promise<CacheTelemetrySummary>;

  closeSession(sessionId: string): Promise<boolean>;
  setVisionOffload(enabled: boolean): Promise<void>;
  setMcpToolBridge(enabled: boolean): Promise<void>;
  setActiveProvider(providerId: string): Promise<{ ok: boolean; active_provider: string }>;
  setOptimizationPolicy(
    update: Partial<OptimizationPolicy> & { mode?: OptimizationMode },
  ): Promise<{ ok: boolean; mode: OptimizationMode; policy: OptimizationPolicy }>;
  testProvider(providerId: string): Promise<Record<string, unknown>>;

  /** Calls changed after `since` (every retained call when omitted). */
  toolCalls(since?: number): Promise<ToolCallsResponse>;
  toolCallDetail(callId: string): Promise<ToolCallDetail>;
  toolCatalog(sessionId: string): Promise<SessionToolCatalog>;

  contextCaptures(): Promise<ContextCapturesResponse>;
  contextCaptureDetail(taskId: string): Promise<ContextCaptureDetailResponse>;
  updateContextCapture(update: ContextCaptureConfigUpdate): Promise<ContextCaptureSummary>;
  clearContextCaptures(): Promise<ContextCaptureSummary>;

  /** Everything recorded, or only the usage within `range`. */
  usage(range?: UsageRange): Promise<UsageResponse>;
  /** Usage per interval between consecutive `bounds` (ascending unix ms, e.g. local midnights). */
  usageSeries(bounds: readonly number[]): Promise<UsageSeriesResponse>;
  resetUsage(): Promise<UsageSummary>;
  pricing(): Promise<PricingResponse>;
  /** `null` restores the default price of `key`. */
  setModelPrice(key: string, price: ModelPrice | null): Promise<PricingResponse>;

  probe(protocol: TaskProtocol, model: string, prompt: string): Promise<string>;
}

/** A half-open time range `[from, to)` in unix ms. */
export interface UsageRange {
  from: number;
  to: number;
}

const RECONNECT_DELAY_MS = 2000;

export class HttpGatewayClient implements GatewayClient {
  readonly demo = false;
  private readonly base: string;
  private readonly key: string;

  constructor(baseUrl: string, apiKey: string) {
    this.base = baseUrl.trim().replace(/\/+$/, "");
    this.key = apiKey.trim();
  }

  get origin(): string {
    if (this.base.length > 0) return this.base;
    if (typeof window !== "undefined" && window.location.port !== "5173") {
      return window.location.origin;
    }
    return "http://127.0.0.1:8787";
  }

  private headers(json = false): Record<string, string> {
    const headers: Record<string, string> = {};
    if (this.key.length > 0) headers.Authorization = `Bearer ${this.key}`;
    if (json) headers["Content-Type"] = "application/json";
    return headers;
  }

  private async request<T>(path: string, init?: { method?: string; body?: unknown }): Promise<T> {
    const hasBody = init?.body !== undefined;
    const resp = await fetch(`${this.base}${path}`, {
      method: init?.method ?? "GET",
      headers: this.headers(hasBody),
      body: hasBody ? JSON.stringify(init?.body) : undefined,
    });
    if (!resp.ok) {
      let detail = `HTTP ${resp.status}`;
      try {
        const body = (await resp.json()) as { error?: { message?: string } };
        if (body.error?.message) detail = body.error.message;
      } catch {
        // Non-JSON error body: the status line is all we have.
      }
      throw new ApiError(resp.status, detail);
    }
    return (await resp.json()) as T;
  }

  subscribe(handlers: SnapshotHandlers): () => void {
    let cancelled = false;
    let source: EventSource | null = null;
    let timer: number | null = null;
    let everLive = false;

    const poll = (): void => {
      this.snapshot().then(
        (snap) => {
          if (cancelled) return;
          handlers.onSnapshot(snap);
          handlers.onError(null);
        },
        (err: unknown) => {
          if (!cancelled) handlers.onError(errorMessage(err));
        },
      );
    };

    const connect = (): void => {
      if (cancelled) return;
      handlers.onState(everLive ? "reconnecting" : "connecting");
      // EventSource cannot send headers, so the key rides the query string.
      const query = this.key.length > 0 ? `?api_key=${encodeURIComponent(this.key)}` : "";
      source = new EventSource(`${this.base}/api/telemetry/events${query}`);
      source.addEventListener("snapshot", (ev: MessageEvent<string>) => {
        if (cancelled) return;
        everLive = true;
        handlers.onState("live");
        handlers.onError(null);
        if (document.visibilityState === "hidden") return;
        try {
          handlers.onSnapshot(JSON.parse(ev.data) as GatewayTelemetrySnapshot);
        } catch (err) {
          handlers.onError(errorMessage(err));
        }
      });
      source.onerror = () => {
        if (cancelled) return;
        source?.close();
        source = null;
        handlers.onState("reconnecting");
        poll();
        timer = window.setTimeout(connect, RECONNECT_DELAY_MS);
      };
    };

    poll();
    connect();

    return () => {
      cancelled = true;
      source?.close();
      if (timer !== null) window.clearTimeout(timer);
    };
  }

  snapshot(): Promise<GatewayTelemetrySnapshot> {
    return this.request("/api/telemetry/snapshot");
  }

  lowCacheHits(): Promise<LowCacheHitsResponse> {
    return this.request("/api/telemetry/low-cache-hits");
  }

  lowCacheHitDetail(taskId: string): Promise<LowCachePayloadDetailResponse> {
    return this.request(`/api/telemetry/low-cache-hits/${encodeURIComponent(taskId)}`);
  }

  clearLowCacheHits(resetTotals: boolean): Promise<CacheTelemetrySummary> {
    return this.request(
      `/api/telemetry/low-cache-hits/clear${resetTotals ? "?reset_totals=true" : ""}`,
      { method: "POST" },
    );
  }

  updateCacheConfig(update: CacheConfigUpdate): Promise<CacheTelemetrySummary> {
    return this.request("/api/telemetry/cache-config", { method: "POST", body: update });
  }

  async closeSession(sessionId: string): Promise<boolean> {
    const body = await this.request<{ closed?: boolean }>(
      `/api/telemetry/sessions/${encodeURIComponent(sessionId)}/close`,
      { method: "POST" },
    );
    return Boolean(body.closed);
  }

  async setVisionOffload(enabled: boolean): Promise<void> {
    await this.request(`/api/telemetry/vision-offload?enabled=${enabled}`, { method: "POST" });
  }

  async setMcpToolBridge(enabled: boolean): Promise<void> {
    await this.request(`/api/telemetry/mcp-tool-bridge?enabled=${enabled}`, { method: "POST" });
  }

  toolCalls(since?: number): Promise<ToolCallsResponse> {
    return this.request(
      since === undefined ? "/api/telemetry/tool-calls" : `/api/telemetry/tool-calls?since=${since}`,
    );
  }

  toolCallDetail(callId: string): Promise<ToolCallDetail> {
    return this.request(`/api/telemetry/tool-calls/${encodeURIComponent(callId)}`);
  }

  toolCatalog(sessionId: string): Promise<SessionToolCatalog> {
    return this.request(`/api/telemetry/sessions/${encodeURIComponent(sessionId)}/tool-catalog`);
  }

  contextCaptures(): Promise<ContextCapturesResponse> {
    return this.request("/api/telemetry/context-captures");
  }

  contextCaptureDetail(taskId: string): Promise<ContextCaptureDetailResponse> {
    return this.request(`/api/telemetry/context-captures/${encodeURIComponent(taskId)}`);
  }

  updateContextCapture(update: ContextCaptureConfigUpdate): Promise<ContextCaptureSummary> {
    return this.request("/api/telemetry/context-capture", { method: "POST", body: update });
  }

  clearContextCaptures(): Promise<ContextCaptureSummary> {
    return this.request("/api/telemetry/context-captures/clear", { method: "POST" });
  }

  usage(range?: UsageRange): Promise<UsageResponse> {
    return this.request(
      range
        ? `/api/telemetry/usage?from=${Math.floor(range.from)}&to=${Math.floor(range.to)}`
        : "/api/telemetry/usage",
    );
  }

  usageSeries(bounds: readonly number[]): Promise<UsageSeriesResponse> {
    return this.request(`/api/telemetry/usage/series?bounds=${bounds.map((b) => Math.floor(b)).join(",")}`);
  }

  resetUsage(): Promise<UsageSummary> {
    return this.request("/api/telemetry/usage/reset", { method: "POST" });
  }

  pricing(): Promise<PricingResponse> {
    return this.request("/api/telemetry/pricing");
  }

  setModelPrice(key: string, price: ModelPrice | null): Promise<PricingResponse> {
    return this.request("/api/telemetry/pricing", { method: "POST", body: { key, price } });
  }

  setActiveProvider(providerId: string): Promise<{ ok: boolean; active_provider: string }> {
    return this.request("/api/telemetry/providers/active", {
      method: "POST",
      body: { provider_id: providerId },
    });
  }

  setOptimizationPolicy(
    update: Partial<OptimizationPolicy> & { mode?: OptimizationMode },
  ): Promise<{ ok: boolean; mode: OptimizationMode; policy: OptimizationPolicy }> {
    return this.request("/api/telemetry/optimization", { method: "POST", body: update });
  }

  testProvider(providerId: string): Promise<Record<string, unknown>> {
    return this.request("/api/telemetry/providers/test", { method: "POST", body: { provider_id: providerId } });
  }

  async probe(protocol: TaskProtocol, model: string, prompt: string): Promise<string> {
    if (protocol === "openai_responses") {
      const data = await this.request<{
        id?: string;
        output?: Array<{ type?: string; content?: Array<{ text?: string }> }>;
      }>("/v1/responses", { method: "POST", body: { model, stream: false, input: prompt } });
      const text =
        data.output?.find((o) => o.type === "message")?.content?.[0]?.text ?? JSON.stringify(data);
      return `[${data.id ?? "ok"}] ${text}`;
    }
    const data = await this.request<{
      id?: string;
      content?: Array<{ type?: string; text?: string }>;
    }>("/v1/messages", {
      method: "POST",
      body: {
        model,
        max_tokens: 1024,
        stream: false,
        messages: [{ role: "user", content: prompt }],
      },
    });
    const text = data.content?.find((c) => c.type === "text")?.text ?? JSON.stringify(data);
    return `[${data.id ?? "ok"}] ${text}`;
  }
}
