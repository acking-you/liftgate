import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage, isUnsupported } from "@/lib/api";

export type ResourceStatus = "idle" | "loading" | "ready" | "unsupported" | "error";

export interface Resource<T> {
  status: ResourceStatus;
  data: T | null;
  error: string | null;
  /** True while a refetch runs on top of data that is already on screen. */
  refreshing: boolean;
  reload: () => void;
}

/**
 * Load a gateway resource. `key` identifies what is being loaded: changing it
 * drops the previous data, while `reload()` refetches in place so the view
 * never flashes back to a skeleton. A 404 resolves to `unsupported`.
 */
export function useResource<T>(load: (() => Promise<T>) | null, key: string): Resource<T> {
  const [state, setState] = useState<{
    key: string;
    status: ResourceStatus;
    data: T | null;
    error: string | null;
  }>({ key, status: load ? "loading" : "idle", data: null, error: null });
  const [nonce, setNonce] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const loadRef = useRef(load);
  loadRef.current = load;
  const enabled = load !== null;

  useEffect(() => {
    const loader = loadRef.current;
    if (!enabled || !loader) {
      setState({ key, status: "idle", data: null, error: null });
      return;
    }
    let cancelled = false;
    setState((prev) =>
      prev.key === key && prev.data !== null
        ? prev
        : { key, status: "loading", data: null, error: null },
    );
    setRefreshing(true);
    loader().then(
      (data) => {
        if (cancelled) return;
        setRefreshing(false);
        setState({ key, status: "ready", data, error: null });
      },
      (err: unknown) => {
        if (cancelled) return;
        setRefreshing(false);
        if (isUnsupported(err)) {
          setState({ key, status: "unsupported", data: null, error: null });
        } else {
          setState((prev) => ({
            key,
            status: "error",
            data: prev.key === key ? prev.data : null,
            error: errorMessage(err),
          }));
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, [key, nonce, enabled]);

  const reload = useCallback(() => setNonce((n) => n + 1), []);

  // A key change is visible one render before the effect resets state.
  if (state.key !== key) {
    return { status: enabled ? "loading" : "idle", data: null, error: null, refreshing, reload };
  }
  return { status: state.status, data: state.data, error: state.error, refreshing, reload };
}

/**
 * A resource that follows a revision counter carried on the telemetry stream:
 * every bump refetches in place, so lists stay current without polling.
 */
export function useRevisionResource<T>(
  load: (() => Promise<T>) | null,
  key: string,
  revision: number | string,
): Resource<T> {
  const resource = useResource(load, key);
  const seen = useRef(revision);
  const { reload } = resource;
  useEffect(() => {
    if (seen.current === revision) return;
    seen.current = revision;
    reload();
  }, [revision, reload]);
  return resource;
}
