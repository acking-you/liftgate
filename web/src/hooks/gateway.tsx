import * as React from "react";
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import {
  type GatewayClient,
  HttpGatewayClient,
  type StreamConnectionState,
  errorMessage,
  isUnsupported,
} from "@/lib/api";
import type {
  GatewayTelemetrySnapshot,
  LowCacheHitRecord,
  ToolCallRecord,
} from "@/types/telemetry";

const STORAGE_BASE_URL = "agy_gateway_base_url";
const STORAGE_API_KEY = "agy_gateway_api_key";

function readStorage(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStorage(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Storage can be unavailable in restricted privacy contexts.
  }
}

interface ConnectionConfig {
  baseUrl: string;
  apiKey: string;
}

let bootstrapped: ConnectionConfig | null = null;

/**
 * `lg` opens the dashboard as `/#api_key=…&base_url=…`. Persist those once and
 * scrub them from the address bar so the key never lingers in history.
 */
function initialConnection(): ConnectionConfig {
  if (bootstrapped) return bootstrapped;
  let hashKey: string | undefined;
  let hashBase: string | undefined;
  const hash = window.location.hash;
  if (hash.startsWith("#") && !hash.startsWith("#/")) {
    const params = new URLSearchParams(hash.slice(1));
    hashKey = params.get("api_key")?.trim() || undefined;
    hashBase = params.get("base_url")?.trim().replace(/\/+$/, "") || undefined;
    if (hashKey !== undefined || hashBase !== undefined) {
      if (hashKey) writeStorage(STORAGE_API_KEY, hashKey);
      if (hashBase) writeStorage(STORAGE_BASE_URL, hashBase);
      params.delete("api_key");
      params.delete("base_url");
      const rest = params.toString();
      window.history.replaceState(
        null,
        "",
        window.location.pathname + window.location.search + (rest ? `#${rest}` : ""),
      );
    }
  }
  const savedBase = readStorage(STORAGE_BASE_URL)?.trim().replace(/\/+$/, "") ?? "";
  bootstrapped = {
    // An empty base URL means "same origin", which is also what the Vite dev proxy expects.
    baseUrl: hashBase ?? savedBase,
    apiKey: hashKey ?? readStorage(STORAGE_API_KEY) ?? "",
  };
  return bootstrapped;
}

/** Skip re-renders for heartbeat snapshots that carry nothing new. */
function isSameSnapshot(prev: GatewayTelemetrySnapshot, next: GatewayTelemetrySnapshot): boolean {
  return (
    next.active_tasks.length === 0 &&
    prev.active_tasks.length === 0 &&
    next.metrics.total_requests === prev.metrics.total_requests &&
    next.metrics.completed_requests === prev.metrics.completed_requests &&
    next.metrics.failed_requests === prev.metrics.failed_requests &&
    next.metrics.cancelled_requests === prev.metrics.cancelled_requests &&
    (next.cache_telemetry?.revision ?? 0) === (prev.cache_telemetry?.revision ?? 0) &&
    (next.tool_call_telemetry?.revision ?? 0) === (prev.tool_call_telemetry?.revision ?? 0) &&
    (next.context_capture?.revision ?? 0) === (prev.context_capture?.revision ?? 0) &&
    next.vision_offload_enabled === prev.vision_offload_enabled &&
    next.mcp_tool_bridge_enabled === prev.mcp_tool_bridge_enabled &&
    next.acp_process.connected === prev.acp_process.connected &&
    next.acp_process.recent_stderr.length === prev.acp_process.recent_stderr.length &&
    next.session_pool.sessions.length === prev.session_pool.sessions.length &&
    next.session_pool.idle_cached_sessions === prev.session_pool.idle_cached_sessions &&
    (next.session_pool.suspended_mcp_sessions ?? 0) ===
      (prev.session_pool.suspended_mcp_sessions ?? 0) &&
    (next.session_pool.mcp_continuations ?? 0) === (prev.session_pool.mcp_continuations ?? 0) &&
    next.available_models.length === prev.available_models.length &&
    next.acp_process.uptime_secs - prev.acp_process.uptime_secs < 10
  );
}

export type ToolCallStreamStatus = "loading" | "ready" | "unsupported" | "error";

export interface GatewayState {
  client: GatewayClient;
  snapshot: GatewayTelemetrySnapshot | null;
  connectionState: StreamConnectionState;
  lastError: string | null;
  baseUrl: string;
  apiKey: string;
  updateConnection: (baseUrl: string, apiKey: string) => void;
  refresh: () => Promise<void>;
  lowCacheRecords: LowCacheHitRecord[];
  lowCacheLoading: boolean;
  reloadLowCache: () => Promise<void>;
  /** Retained tool calls, oldest first, kept current from `?since=` deltas. */
  toolCalls: ToolCallRecord[];
  toolCallsStatus: ToolCallStreamStatus;
  toolCallsError: string | null;
  reloadToolCalls: () => void;
}

const GatewayContext = createContext<GatewayState | null>(null);

export function GatewayProvider({
  demoClient,
  children,
}: {
  /** Dev-only synthetic backend; when set the HTTP client is never constructed. */
  demoClient?: GatewayClient;
  children: React.ReactNode;
}): React.JSX.Element {
  const [config, setConfig] = useState<ConnectionConfig>(initialConnection);
  const client = useMemo<GatewayClient>(
    () => demoClient ?? new HttpGatewayClient(config.baseUrl, config.apiKey),
    [demoClient, config],
  );

  const [snapshot, setSnapshot] = useState<GatewayTelemetrySnapshot | null>(null);
  const [connectionState, setConnectionState] = useState<StreamConnectionState>("connecting");
  const [lastError, setLastError] = useState<string | null>(null);
  const [lowCacheRecords, setLowCacheRecords] = useState<LowCacheHitRecord[]>([]);
  const [lowCacheLoading, setLowCacheLoading] = useState(false);
  const fetchedCacheRevision = useRef(-1);

  const [toolCallMap, setToolCallMap] = useState<ReadonlyMap<string, ToolCallRecord>>(
    () => new Map(),
  );
  const [toolCallsStatus, setToolCallsStatus] = useState<ToolCallStreamStatus>("loading");
  const [toolCallsError, setToolCallsError] = useState<string | null>(null);
  /** Revision covered by `toolCallMap`; -1 forces a full fetch. */
  const fetchedToolRevision = useRef(-1);
  /** Client whose fetch is outstanding; a new client never waits on an old client's request. */
  const toolFetchOwner = useRef<GatewayClient | null>(null);
  /** Another pass was requested while the owner's fetch was outstanding. */
  const toolSyncQueued = useRef(false);
  /** The next pass refetches everything instead of a `?since=` delta. */
  const toolFullReload = useRef(false);
  const activeClient = useRef(client);
  activeClient.current = client;

  const syncToolCalls = useCallback(async (): Promise<void> => {
    if (toolFetchOwner.current === client) {
      // The running loop picks this up before it finishes.
      toolSyncQueued.current = true;
      return;
    }
    toolFetchOwner.current = client;
    try {
      for (;;) {
        toolSyncQueued.current = false;
        const full = toolFullReload.current || fetchedToolRevision.current < 0;
        toolFullReload.current = false;
        const since = full ? undefined : fetchedToolRevision.current;
        const data = await client.toolCalls(since);
        if (activeClient.current !== client) return;
        const { revision, oldest_seq } = data.summary;
        if (since !== undefined && revision < since) {
          // The gateway restarted and its revisions started over: refetch everything.
          toolFullReload.current = true;
          continue;
        }
        setToolCallMap((prev) => {
          const next = new Map(since === undefined ? [] : prev);
          for (const rec of data.records) next.set(rec.call_id, rec);
          for (const [id, rec] of next) {
            if (rec.seq < oldest_seq) next.delete(id);
          }
          return next;
        });
        fetchedToolRevision.current = revision;
        if (!toolSyncQueued.current && !toolFullReload.current) break;
      }
      setToolCallsStatus("ready");
      setToolCallsError(null);
    } catch (err) {
      if (activeClient.current !== client) return;
      if (isUnsupported(err)) {
        setToolCallsStatus("unsupported");
      } else {
        setToolCallsStatus((prev) => (prev === "ready" ? prev : "error"));
        setToolCallsError(errorMessage(err));
      }
    } finally {
      if (toolFetchOwner.current === client) toolFetchOwner.current = null;
    }
  }, [client]);

  const reloadToolCalls = useCallback((): void => {
    toolFullReload.current = true;
    void syncToolCalls();
  }, [syncToolCalls]);

  const reloadLowCache = useCallback(async (): Promise<void> => {
    setLowCacheLoading(true);
    try {
      const data = await client.lowCacheHits();
      setLowCacheRecords(data.records ?? []);
      fetchedCacheRevision.current = data.summary?.revision ?? -1;
    } catch {
      // The snapshot stream already surfaces connectivity problems.
    } finally {
      setLowCacheLoading(false);
    }
  }, [client]);

  const refresh = useCallback(async (): Promise<void> => {
    try {
      setSnapshot(await client.snapshot());
      setLastError(null);
    } catch (err) {
      setLastError(errorMessage(err));
    }
  }, [client]);

  useEffect(() => {
    fetchedCacheRevision.current = -1;
    fetchedToolRevision.current = -1;
    toolSyncQueued.current = false;
    toolFullReload.current = false;
    setSnapshot(null);
    setLowCacheRecords([]);
    setToolCallMap(new Map());
    setToolCallsStatus("loading");
    setToolCallsError(null);
    setConnectionState("connecting");
    return client.subscribe({
      onSnapshot: (next) =>
        setSnapshot((prev) => (prev !== null && isSameSnapshot(prev, next) ? prev : next)),
      onState: setConnectionState,
      onError: setLastError,
    });
  }, [client]);

  const cacheRevision = snapshot?.cache_telemetry?.revision ?? -1;
  useEffect(() => {
    if (cacheRevision >= 0 && cacheRevision !== fetchedCacheRevision.current) {
      void reloadLowCache();
    }
  }, [cacheRevision, reloadLowCache]);

  const hasSnapshot = snapshot !== null;
  const toolRevision = snapshot?.tool_call_telemetry?.revision;
  useEffect(() => {
    if (!hasSnapshot) return;
    if (toolRevision === undefined) {
      // The gateway predates per-call tool telemetry.
      setToolCallsStatus("unsupported");
      return;
    }
    if (toolRevision < fetchedToolRevision.current) {
      // Revision went backwards: the gateway restarted, so start over.
      fetchedToolRevision.current = -1;
    }
    if (toolRevision !== fetchedToolRevision.current) void syncToolCalls();
  }, [hasSnapshot, toolRevision, syncToolCalls]);

  const toolCalls = useMemo(
    () => [...toolCallMap.values()].sort((a, b) => a.seq - b.seq),
    [toolCallMap],
  );

  const updateConnection = useCallback((baseUrl: string, apiKey: string): void => {
    const next = { baseUrl: baseUrl.trim().replace(/\/+$/, ""), apiKey: apiKey.trim() };
    writeStorage(STORAGE_BASE_URL, next.baseUrl);
    writeStorage(STORAGE_API_KEY, next.apiKey);
    bootstrapped = next;
    setConfig(next);
  }, []);

  const value = useMemo<GatewayState>(
    () => ({
      client,
      snapshot,
      connectionState,
      lastError,
      baseUrl: config.baseUrl,
      apiKey: config.apiKey,
      updateConnection,
      refresh,
      lowCacheRecords,
      lowCacheLoading,
      reloadLowCache,
      toolCalls,
      toolCallsStatus,
      toolCallsError,
      reloadToolCalls,
    }),
    [
      client,
      snapshot,
      connectionState,
      lastError,
      config,
      updateConnection,
      refresh,
      lowCacheRecords,
      lowCacheLoading,
      reloadLowCache,
      toolCalls,
      toolCallsStatus,
      toolCallsError,
      reloadToolCalls,
    ],
  );

  return <GatewayContext.Provider value={value}>{children}</GatewayContext.Provider>;
}

export function useGateway(): GatewayState {
  const ctx = useContext(GatewayContext);
  if (!ctx) throw new Error("useGateway must be used inside <GatewayProvider>");
  return ctx;
}
