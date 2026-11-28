export interface ProviderMeta {
  id: string;
  label: string;
  short: string;
  color: string;
}

const KNOWN_PROVIDERS: Readonly<Record<string, ProviderMeta>> = {
  agy: {
    id: "agy",
    label: "Antigravity (agy)",
    short: "agy",
    color: "#06b6d4",
  },
  openai: {
    id: "openai",
    label: "OpenAI Platform",
    short: "openai",
    color: "#10b981",
  },
  anthropic: {
    id: "anthropic",
    label: "Anthropic Platform",
    short: "anthropic",
    color: "#d97706",
  },
  gemini: {
    id: "gemini",
    label: "Google Gemini API",
    short: "gemini",
    color: "#3b82f6",
  },
  "codex-app-server": {
    id: "codex-app-server",
    label: "Codex App-Server",
    short: "codex-acp",
    color: "#14b8a6",
  },
  "grok-acp": {
    id: "grok-acp",
    label: "Grok ACP",
    short: "grok-acp",
    color: "#8b5cf6",
  },
};

const PROVIDER_ORDER = ["agy", "openai", "anthropic", "gemini", "codex-app-server", "grok-acp"];

export function providerMeta(id: string | null | undefined): ProviderMeta {
  const clean = (id ?? "agy").trim().toLowerCase() || "agy";
  const known = KNOWN_PROVIDERS[clean];
  if (known) return known;
  return {
    id: clean,
    label: id?.trim() || "agy",
    short: id?.trim() || "agy",
    color: "#64748b",
  };
}

export function sortProviderIds(ids: readonly string[]): string[] {
  return [...ids].sort((a, b) => {
    const ia = PROVIDER_ORDER.indexOf(a);
    const ib = PROVIDER_ORDER.indexOf(b);
    if (ia !== -1 && ib !== -1) return ia - ib;
    if (ia !== -1) return -1;
    if (ib !== -1) return 1;
    return a.localeCompare(b);
  });
}
