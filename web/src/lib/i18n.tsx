import * as React from "react";
import { useCallback, useContext, useEffect, useMemo, useState } from "react";
import { type Messages, catalogs } from "@/i18n";
import { I18nContext, type I18nState, type Locale } from "@/lib/i18n-context";

export type { Locale, Messages };

const STORAGE_KEY = "lg_locale";

/** Chinese when the browser's preferred language is any Chinese variant, English otherwise. */
export function systemLocale(): Locale {
  if (typeof navigator === "undefined") return "en";
  const preferred = navigator.languages?.[0] ?? navigator.language ?? "en";
  return /^zh\b/i.test(preferred) ? "zh" : "en";
}

function storedLocale(): Locale | null {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY);
    return value === "zh" || value === "en" ? value : null;
  } catch {
    return null;
  }
}

let currentLocale: Locale = storedLocale() ?? systemLocale();

/**
 * Messages of the active locale for code outside React components (formatters, label maps).
 * Components should prefer `useI18n()` so they re-render on a switch.
 */
export function msg(): Messages {
  return catalogs[currentLocale];
}

export function activeLocale(): Locale {
  return currentLocale;
}

export function I18nProvider({ children }: { children: React.ReactNode }): React.JSX.Element {
  const [preference, setPreference] = useState<Locale | null>(storedLocale);
  const [system, setSystem] = useState<Locale>(systemLocale);

  useEffect(() => {
    const onChange = (): void => setSystem(systemLocale());
    window.addEventListener("languagechange", onChange);
    return () => window.removeEventListener("languagechange", onChange);
  }, []);

  const locale = preference ?? system;
  // Updated during render so helpers called while rendering this tree see the new language.
  currentLocale = locale;
  const m = catalogs[locale];

  useEffect(() => {
    document.documentElement.lang = locale === "zh" ? "zh-CN" : "en";
  }, [locale]);

  const setLocale = useCallback(
    (next: Locale) => {
      const follow = next === system;
      try {
        if (follow) window.localStorage.removeItem(STORAGE_KEY);
        else window.localStorage.setItem(STORAGE_KEY, next);
      } catch {
        // The choice simply does not persist when storage is unavailable.
      }
      setPreference(follow ? null : next);
    },
    [system],
  );

  const value = useMemo<I18nState>(() => ({ locale, m, setLocale }), [locale, m, setLocale]);
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nState {
  const ctx = useContext(I18nContext);
  if (!ctx) throw new Error("useI18n must be used inside <I18nProvider>");
  return ctx;
}
