import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  CacheTelemetrySummary,
  GatewayTelemetrySnapshot,
  LowCacheHitRecord,
  LowCacheHitsResponse,
} from "@/types/telemetry";

export type StreamConnectionState = "connecting" | "live" | "reconnecting" | "error";

const STORAGE_BASE_URL = "agy_gateway_base_url";
const STORAGE_API_KEY = "agy_gateway_api_key";

interface BootstrapParams {
  baseUrl?: string;
  apiKey?: string;
}

let cachedBootstrapParams: BootstrapParams | null = null;

function safeGetStorage(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function safeSetStorage(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Ignore storage write errors in restricted privacy contexts.
  }
}

function getBootstrapHashParams(): BootstrapParams {
  if (cachedBootstrapParams !== null) {
    return cachedBootstrapParams;
  }
  if (typeof window === "undefined" || !window.location.hash.startsWith("#")) {
    cachedBootstrapParams = {};
    return cachedBootstrapParams;
  }
  const params = new URLSearchParams(window.location.hash.slice(1));
  const rawKey = params.get("api_key");
  const rawBase = params.get("base_url");
  if (rawKey === null && rawBase === null) {
    cachedBootstrapParams = {};
    return cachedBootstrapParams;
  }
  const cleanKey = rawKey?.trim() || undefined;
  const cleanBase = rawBase?.trim().replace(/\/+$/, "") || undefined;
  if (cleanKey) {
    safeSetStorage(STORAGE_API_KEY, cleanKey);
    params.delete("api_key");
  }
  if (cleanBase) {
    safeSetStorage(STORAGE_BASE_URL, cleanBase);
    params.delete("base_url");
  }
  const remaining = params.toString();
  const nextUrl =
    window.location.pathname +
    window.location.search +
    (remaining.length > 0 ? `#${remaining}` : "");
  window.history.replaceState(null, "", nextUrl);
  cachedBootstrapParams = { baseUrl: cleanBase, apiKey: cleanKey };
  return cachedBootstrapParams;
}

function defaultGatewayBaseUrl(): string {
  if (typeof window === "undefined") {
    return "http://127.0.0.1:8787";
  }
  const boot = getBootstrapHashParams();
  if (boot.baseUrl) {
    return boot.baseUrl;
  }
  const saved = safeGetStorage(STORAGE_BASE_URL);
  if (saved !== null && saved.trim().length > 0) {
    return saved.trim().replace(/\/+$/, "");
  }
  if (window.location.port === "5173") {
    return "";
  }
  return window.location.origin;
}

export interface GatewayTelemetryState {
  snapshot: GatewayTelemetrySnapshot | null;
  lowCacheRecords: LowCacheHitRecord[];
  lowCacheLoading: boolean;
  connectionState: StreamConnectionState;
  lastError: string | null;
  baseUrl: string;
  apiKey: string;
  updateConnectionConfig: (nextBaseUrl: string, nextApiKey: string) => void;
  refreshSnapshot: () => Promise<void>;
  refreshLowCacheHits: () => Promise<void>;
  closeIdleSession: (sessionId: string) => Promise<boolean>;
  toggleVisionOffload: (enabled?: boolean) => Promise<boolean | null>;
  updateCacheConfig: (
    maxLowCacheRecords: number,
    lowCacheThresholdPct: number,
  ) => Promise<{ ok: true; summary: CacheTelemetrySummary } | { ok: false; error: string }>;
  clearLowCacheHits: (resetTotals?: boolean) => Promise<CacheTelemetrySummary | null>;
}

export function useGatewayTelemetry(): GatewayTelemetryState {
  const [baseUrl, setBaseUrl] = useState<string>(defaultGatewayBaseUrl);
  const [apiKey, setApiKey] = useState<string>(() => {
    if (typeof window === "undefined") return "";
    const boot = getBootstrapHashParams();
    if (boot.apiKey) return boot.apiKey;
    return safeGetStorage(STORAGE_API_KEY) ?? "";
  });

  const [snapshot, setSnapshot] = useState<GatewayTelemetrySnapshot | null>(null);
  const [lowCacheRecords, setLowCacheRecords] = useState<LowCacheHitRecord[]>([]);
  const [lowCacheLoading, setLowCacheLoading] = useState<boolean>(false);
  const [connectionState, setConnectionState] =
    useState<StreamConnectionState>("connecting");
  const [lastError, setLastError] = useState<string | null>(null);
  const reconnectTimerRef = useRef<number | null>(null);
  const lastFetchedCacheRevisionRef = useRef<number>(-1);

  const buildUrl = useCallback(
    (path: string, includeQueryKey = false): string => {
      const normalizedBase = baseUrl.trim().replace(/\/+$/, "");
      const baseAndPath = `${normalizedBase}${path}`;
      if (includeQueryKey && apiKey.trim().length > 0) {
        const sep = baseAndPath.includes("?") ? "&" : "?";
        return `${baseAndPath}${sep}api_key=${encodeURIComponent(apiKey.trim())}`;
      }
      return baseAndPath;
    },
    [baseUrl, apiKey],
  );

  const authHeaders = useMemo<Record<string, string>>(() => {
    const headers: Record<string, string> = {};
    if (apiKey.trim().length > 0) {
      headers["Authorization"] = `Bearer ${apiKey.trim()}`;
    }
    return headers;
  }, [apiKey]);

  const refreshLowCacheHits = useCallback(async (): Promise<void> => {
    setLowCacheLoading(true);
    try {
      const resp = await fetch(buildUrl("/api/telemetry/low-cache-hits"), {
        headers: authHeaders,
      });
      if (!resp.ok) {
        return;
      }
      const data = (await resp.json()) as LowCacheHitsResponse;
      setLowCacheRecords(data.records ?? []);
      if (data.summary) {
        lastFetchedCacheRevisionRef.current = data.summary.revision;
      }
    } catch {
      // Snapshot SSE loop already surfaces connectivity errors cleanly.
    } finally {
      setLowCacheLoading(false);
    }
  }, [authHeaders, buildUrl]);

  const refreshSnapshot = useCallback(async (): Promise<void> => {
    try {
      const resp = await fetch(buildUrl("/api/telemetry/snapshot"), {
        headers: authHeaders,
      });
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status} ${resp.statusText}`);
      }
      const data = (await resp.json()) as GatewayTelemetrySnapshot;
      setSnapshot(data);
      setLastError(null);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setLastError(msg);
    }
  }, [authHeaders, buildUrl]);

  const cacheRevision = snapshot?.cache_telemetry?.revision ?? -1;
  useEffect(() => {
    if (cacheRevision >= 0 && cacheRevision !== lastFetchedCacheRevisionRef.current) {
      void refreshLowCacheHits();
    }
  }, [cacheRevision, refreshLowCacheHits]);

  const closeIdleSession = useCallback(
    async (sessionId: string): Promise<boolean> => {
      const resp = await fetch(
        buildUrl(`/api/telemetry/sessions/${encodeURIComponent(sessionId)}/close`),
        {
          method: "POST",
          headers: authHeaders,
        },
      );
      if (!resp.ok) {
        return false;
      }
      const body = (await resp.json()) as { closed?: boolean };
      await refreshSnapshot();
      return Boolean(body.closed);
    },
    [authHeaders, buildUrl, refreshSnapshot],
  );

  const toggleVisionOffload = useCallback(
    async (enabled?: boolean): Promise<boolean | null> => {
      const query =
        typeof enabled === "boolean" ? `?enabled=${enabled ? "true" : "false"}` : "";
      const resp = await fetch(buildUrl(`/api/telemetry/vision-offload${query}`), {
        method: "POST",
        headers: authHeaders,
      });
      if (!resp.ok) {
        return null;
      }
      const body = (await resp.json()) as { vision_offload_enabled?: boolean };
      await refreshSnapshot();
      return typeof body.vision_offload_enabled === "boolean"
        ? body.vision_offload_enabled
        : null;
    },
    [authHeaders, buildUrl, refreshSnapshot],
  );

  const updateCacheConfig = useCallback(
    async (
      maxLowCacheRecords: number,
      lowCacheThresholdPct: number,
    ): Promise<
      { ok: true; summary: CacheTelemetrySummary } | { ok: false; error: string }
    > => {
      try {
        const resp = await fetch(buildUrl("/api/telemetry/cache-config"), {
          method: "POST",
          headers: {
            ...authHeaders,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({
            max_low_cache_records: maxLowCacheRecords,
            low_cache_threshold_pct: lowCacheThresholdPct,
          }),
        });
        const payload = (await resp.json()) as
          | CacheTelemetrySummary
          | { error?: { message?: string } };
        if (!resp.ok) {
          const errMsg =
            "error" in payload && payload.error?.message
              ? payload.error.message
              : `HTTP ${resp.status}`;
          return { ok: false, error: errMsg };
        }
        const summary = payload as CacheTelemetrySummary;
        await Promise.all([refreshSnapshot(), refreshLowCacheHits()]);
        return { ok: true, summary };
      } catch (err) {
        return {
          ok: false,
          error: err instanceof Error ? err.message : String(err),
        };
      }
    },
    [authHeaders, buildUrl, refreshLowCacheHits, refreshSnapshot],
  );

  const clearLowCacheHits = useCallback(
    async (resetTotals = false): Promise<CacheTelemetrySummary | null> => {
      const query = resetTotals ? "?reset_totals=true" : "";
      const resp = await fetch(
        buildUrl(`/api/telemetry/low-cache-hits/clear${query}`),
        {
          method: "POST",
          headers: authHeaders,
        },
      );
      if (!resp.ok) {
        return null;
      }
      const summary = (await resp.json()) as CacheTelemetrySummary;
      await Promise.all([refreshSnapshot(), refreshLowCacheHits()]);
      return summary;
    },
    [authHeaders, buildUrl, refreshLowCacheHits, refreshSnapshot],
  );

  const updateConnectionConfig = useCallback(
    (nextBaseUrl: string, nextApiKey: string): void => {
      const cleanUrl = nextBaseUrl.trim().replace(/\/+$/, "");
      const cleanKey = nextApiKey.trim();
      lastFetchedCacheRevisionRef.current = -1;
      setBaseUrl(cleanUrl);
      setApiKey(cleanKey);
      if (typeof window !== "undefined") {
        safeSetStorage(STORAGE_BASE_URL, cleanUrl);
        safeSetStorage(STORAGE_API_KEY, cleanKey);
      }
    },
    [],
  );

  useEffect(() => {
    let cancelled = false;
    let es: EventSource | null = null;

    const connect = (): void => {
      if (cancelled) return;
      setConnectionState((prev) => (prev === "live" ? "reconnecting" : "connecting"));

      const sseUrl = buildUrl("/api/telemetry/events", true);
      es = new EventSource(sseUrl);

      es.addEventListener("snapshot", (ev: MessageEvent<string>) => {
        if (cancelled) return;
        setConnectionState("live");
        setLastError(null);
        if (typeof document !== "undefined" && document.visibilityState === "hidden") {
          return;
        }
        try {
          const parsed = JSON.parse(ev.data) as GatewayTelemetrySnapshot;
          setSnapshot((prev) => {
            if (
              prev !== null &&
              parsed.active_tasks.length === 0 &&
              prev.active_tasks.length === 0 &&
              parsed.metrics.total_requests === prev.metrics.total_requests &&
              parsed.metrics.completed_requests === prev.metrics.completed_requests &&
              parsed.metrics.failed_requests === prev.metrics.failed_requests &&
              (parsed.cache_telemetry?.revision ?? 0) ===
                (prev.cache_telemetry?.revision ?? 0) &&
              parsed.vision_offload_enabled === prev.vision_offload_enabled &&
              parsed.acp_process.connected === prev.acp_process.connected &&
              parsed.acp_process.recent_stderr.length ===
                prev.acp_process.recent_stderr.length &&
              parsed.session_pool.sessions.length ===
                prev.session_pool.sessions.length &&
              parsed.available_models.length === prev.available_models.length &&
              parsed.acp_process.uptime_secs - prev.acp_process.uptime_secs < 10
            ) {
              return prev;
            }
            return parsed;
          });
        } catch (parseErr) {
          setLastError(
            parseErr instanceof Error ? parseErr.message : "Invalid SSE JSON",
          );
        }
      });

      es.onerror = () => {
        if (cancelled) return;
        es?.close();
        es = null;
        setConnectionState("reconnecting");
        void refreshSnapshot();
        if (reconnectTimerRef.current !== null) {
          window.clearTimeout(reconnectTimerRef.current);
        }
        reconnectTimerRef.current = window.setTimeout(() => {
          connect();
        }, 2000);
      };
    };

    void refreshSnapshot();
    void refreshLowCacheHits();
    connect();

    return () => {
      cancelled = true;
      es?.close();
      if (reconnectTimerRef.current !== null) {
        window.clearTimeout(reconnectTimerRef.current);
      }
    };
  }, [buildUrl, refreshLowCacheHits, refreshSnapshot]);

  return {
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
  };
}
