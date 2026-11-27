import { useCallback, useEffect, useState } from "react";

export const VIEW_IDS = [
  "overview",
  "usage",
  "tools",
  "catalog",
  "context",
  "cache",
  "sessions",
  "models",
  "diagnostics",
] as const;

export type ViewId = (typeof VIEW_IDS)[number];

export interface Route {
  view: ViewId;
  /** Optional selection carried in the URL, e.g. a session id for the catalog. */
  arg: string | null;
}

function parseHash(hash: string): Route {
  const match = /^#\/([a-z]+)(?:\/(.+))?$/.exec(hash);
  const view = VIEW_IDS.find((id) => id === match?.[1]) ?? "overview";
  const rawArg = match?.[2];
  let arg: string | null = null;
  if (rawArg) {
    try {
      arg = decodeURIComponent(rawArg);
    } catch {
      arg = rawArg;
    }
  }
  return { view, arg };
}

/** Hash routing keeps every view linkable while the gateway serves a single static page. */
export function useHashRoute(): Route & { navigate: (view: ViewId, arg?: string | null) => void } {
  const [route, setRoute] = useState<Route>(() => parseHash(window.location.hash));

  useEffect(() => {
    const onChange = (): void => setRoute(parseHash(window.location.hash));
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);

  const navigate = useCallback((view: ViewId, arg?: string | null) => {
    const next = `#/${view}${arg ? `/${encodeURIComponent(arg)}` : ""}`;
    if (window.location.hash === next) return;
    window.location.hash = next;
  }, []);

  return { ...route, navigate };
}
