import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { GatewayTelemetrySnapshot } from "@/types/telemetry";

export type StreamConnectionState = "connecting" | "live" | "reconnecting" | "error";

const STORAGE_BASE_URL = "agy_gateway_base_url";
const STORAGE_API_KEY = "agy_gateway_api_key";

function consumeBootstrapHashParams(): { baseUrl?: string; apiKey?: string } {
  if (typeof window === "undefined" || !window.location.hash.startsWith("#")) {
    return {};
  }
  const params = new URLSearchParams(window.location.hash.slice(1));
  const rawKey = params.get("api_key");
  const rawBase = params.get("base_url");
  if (rawKey === null && rawBase === null) {
    return {};
  }
  if (rawKey !== null && rawKey.trim().length > 0) {
    window.localStorage.setItem(STORAGE_API_KEY, rawKey.trim());
    params.delete("api_key");
  }
  if (rawBase !== null && rawBase.trim().length > 0) {
    window.localStorage.setItem(
      STORAGE_BASE_URL,
      rawBase.trim().replace(/\/+$/, ""),
    );
    params.delete("base_url");
  }
  const remaining = params.toString();
  const nextUrl =
    window.location.pathname +
    window.location.search +
    (remaining.length > 0 ? `#${remaining}` : "");
  window.history.replaceState(null, "", nextUrl);
  return {
    baseUrl: rawBase?.trim().replace(/\/+$/, "") || undefined,
    apiKey: rawKey?.trim() || undefined,
  };
}

function defaultGatewayBaseUrl(): string {
  if (typeof window === "undefined") {
    return "http://127.0.0.1:8787";
  }
  const boot = consumeBootstrapHashParams();
  if (boot.baseUrl) {
    return boot.baseUrl;
  }
  const saved = window.localStorage.getItem(STORAGE_BASE_URL);
  if (saved !== null && saved.trim().length > 0) {
    return saved.trim().replace(/\/+$/, "");
  }
  // If running under Vite dev server on :5173, use empty base so Vite proxy or explicit URL works;
  // if co-hosted directly on the Rust gateway port, window.location.origin is exact!
  if (window.location.port === "5173") {
    return "";
  }
  return window.location.origin;
}

export interface GatewayTelemetryState {
  snapshot: GatewayTelemetrySnapshot | null;
  connectionState: StreamConnectionState;
  lastError: string | null;
  baseUrl: string;
  apiKey: string;
  updateConnectionConfig: (nextBaseUrl: string, nextApiKey: string) => void;
  refreshSnapshot: () => Promise<void>;
  closeIdleSession: (sessionId: string) => Promise<boolean>;
}

export function useGatewayTelemetry(): GatewayTelemetryState {
  const [baseUrl, setBaseUrl] = useState<string>(defaultGatewayBaseUrl);
  const [apiKey, setApiKey] = useState<string>(() => {
    if (typeof window === "undefined") return "";
    const boot = consumeBootstrapHashParams();
    if (boot.apiKey) return boot.apiKey;
    return window.localStorage.getItem(STORAGE_API_KEY) ?? "";
  });

  const [snapshot, setSnapshot] = useState<GatewayTelemetrySnapshot | null>(null);
  const [connectionState, setConnectionState] =
    useState<StreamConnectionState>("connecting");
  const [lastError, setLastError] = useState<string | null>(null);
  const reconnectTimerRef = useRef<number | null>(null);

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

  const authHeaders = useMemo< Record<string, string> >(() => {
    const headers: Record<string, string> = {};
    if (apiKey.trim().length > 0) {
      headers["Authorization"] = `Bearer ${apiKey.trim()}`;
    }
    return headers;
  }, [apiKey]);

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

  const updateConnectionConfig = useCallback(
    (nextBaseUrl: string, nextApiKey: string): void => {
      const cleanUrl = nextBaseUrl.trim().replace(/\/+$/, "");
      const cleanKey = nextApiKey.trim();
      setBaseUrl(cleanUrl);
      setApiKey(cleanKey);
      if (typeof window !== "undefined") {
        window.localStorage.setItem(STORAGE_BASE_URL, cleanUrl);
        window.localStorage.setItem(STORAGE_API_KEY, cleanKey);
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
        try {
          const parsed = JSON.parse(ev.data) as GatewayTelemetrySnapshot;
          setSnapshot(parsed);
          setConnectionState("live");
          setLastError(null);
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
    connect();

    return () => {
      cancelled = true;
      es?.close();
      if (reconnectTimerRef.current !== null) {
        window.clearTimeout(reconnectTimerRef.current);
      }
    };
  }, [buildUrl, refreshSnapshot]);

  return {
    snapshot,
    connectionState,
    lastError,
    baseUrl,
    apiKey,
    updateConnectionConfig,
    refreshSnapshot,
    closeIdleSession,
  };
}
