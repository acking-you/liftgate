import { msg } from "@/lib/i18n";

/**
 * Harness identity. Colour follows the harness, never its rank, so a filter that
 * hides one harness leaves the others painted exactly as before.
 */
export interface HarnessMeta {
  id: string;
  label: string;
  /** CSS colour reference for the identity swatch. */
  color: string;
}

const KNOWN: Record<string, HarnessMeta> = {
  codex: { id: "codex", label: "Codex", color: "var(--lg-s1)" },
  "claude-code": { id: "claude-code", label: "Claude Code", color: "var(--lg-s2)" },
  opencode: { id: "opencode", label: "OpenCode", color: "var(--lg-s3)" },
  omp: { id: "omp", label: "Oh My Pi", color: "var(--lg-s4)" },
  generic: { id: "generic", label: "Generic", color: "var(--lg-s-neutral)" },
};

export const HARNESS_ORDER: readonly string[] = [
  "codex",
  "claude-code",
  "opencode",
  "omp",
  "generic",
];

export function harnessMeta(id: string | null | undefined): HarnessMeta {
  if (!id) return { id: "unknown", label: msg().common.harnessUnknown, color: "var(--lg-s-neutral)" };
  return KNOWN[id] ?? { id, label: id, color: "var(--lg-s-neutral)" };
}

/** Sort harness ids in the fixed identity order, unknown ids last (alphabetical). */
export function sortHarnessIds(ids: Iterable<string>): string[] {
  return [...new Set(ids)].sort((a, b) => {
    const ia = HARNESS_ORDER.indexOf(a);
    const ib = HARNESS_ORDER.indexOf(b);
    if (ia !== -1 || ib !== -1) {
      return (ia === -1 ? 99 : ia) - (ib === -1 ? 99 : ib);
    }
    return a.localeCompare(b);
  });
}

export function orchestratorModeLabel(mode: string | null | undefined): string {
  switch (mode) {
    case "codex_exec":
      return "exec";
    case "claude_bash_first":
      return msg().common.modeBashFirst;
    case "opencode_execute_full":
      return "execute · Full";
    case "opencode_execute":
      return "execute · JS";
    case "omp_eval":
      return "eval";
    case "direct":
    case null:
    case undefined:
      return msg().common.modeDirect;
    default:
      return mode;
  }
}
