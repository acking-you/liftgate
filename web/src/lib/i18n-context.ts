import { createContext } from "react";
import type { Messages } from "@/i18n";

export type Locale = "zh" | "en";

export interface I18nState {
  locale: Locale;
  m: Messages;
  /** Switch language; picking the system language goes back to following the system. */
  setLocale: (next: Locale) => void;
}

/**
 * Lives apart from the catalogs so editing a message file during development hot-reloads the
 * strings without minting a new context the already-mounted provider does not know about.
 */
export const I18nContext = createContext<I18nState | null>(null);
