import * as React from "react";
import { useI18n } from "@/lib/i18n";

/** Flips between Chinese and English; choosing the system language resumes following it. */
export function LocaleToggle(): React.JSX.Element {
  const { m, locale, setLocale } = useI18n();
  return (
    <button
      type="button"
      className="lg-btn lg-locale"
      data-variant="ghost"
      data-icon="true"
      title={m.common.localeSwitch}
      aria-label={m.common.localeSwitch}
      onClick={() => setLocale(locale === "zh" ? "en" : "zh")}
    >
      <span aria-hidden="true">{m.common.localeShort}</span>
    </button>
  );
}
