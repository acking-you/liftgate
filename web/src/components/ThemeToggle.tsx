import * as React from "react";
import { useEffect, useRef, useState } from "react";
import { Moon, Sun } from "lucide-react";
import { useI18n } from "@/lib/i18n";

declare global {
  interface Window {
    __toggleTheme?: (evt: MouseEvent, originEl: HTMLElement | null) => void;
  }
}

function readIsDark(): boolean {
  return document.documentElement.getAttribute("data-theme") === "dark";
}

export function ThemeToggle(): React.JSX.Element {
  const { m } = useI18n();
  const [isDark, setIsDark] = useState<boolean>(readIsDark);
  const btnRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    const handler = (ev: Event): void => {
      const theme = (ev as CustomEvent<{ theme?: string }>).detail?.theme;
      setIsDark(theme ? theme === "dark" : readIsDark());
    };
    window.addEventListener("lg-theme-change", handler);
    return () => window.removeEventListener("lg-theme-change", handler);
  }, []);

  const label = isDark ? m.common.themeToLight : m.common.themeToDark;

  return (
    <button
      ref={btnRef}
      type="button"
      className="lg-btn"
      data-variant="ghost"
      data-icon="true"
      title={label}
      aria-label={label}
      onClick={(ev) => {
        if (typeof window.__toggleTheme === "function") {
          window.__toggleTheme(ev.nativeEvent, btnRef.current);
          return;
        }
        const next = isDark ? "light" : "dark";
        document.documentElement.setAttribute("data-theme", next);
        try {
          window.localStorage.setItem("theme", next);
        } catch {
          // Preference simply does not persist when storage is unavailable.
        }
        setIsDark(!isDark);
      }}
    >
      {isDark ? <Sun /> : <Moon />}
    </button>
  );
}
