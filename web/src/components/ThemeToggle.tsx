import * as React from "react";
import { useEffect, useId, useRef, useState } from "react";

declare global {
  interface Window {
    __toggleTheme?: (evt: MouseEvent, originEl: HTMLElement | null) => void;
  }
}

const SUN_RAYS: ReadonlyArray<{
  x1: string;
  y1: string;
  x2: string;
  y2: string;
  idx: number;
}> = [
  { x1: "12", y1: "2.2", x2: "12", y2: "4.4", idx: 0 },
  { x1: "17.37", y1: "6.63", x2: "18.93", y2: "5.07", idx: 1 },
  { x1: "19.6", y1: "12", x2: "21.8", y2: "12", idx: 2 },
  { x1: "17.37", y1: "17.37", x2: "18.93", y2: "18.93", idx: 3 },
  { x1: "12", y1: "19.6", x2: "12", y2: "21.8", idx: 4 },
  { x1: "6.63", y1: "17.37", x2: "5.07", y2: "18.93", idx: 5 },
  { x1: "2.2", y1: "12", x2: "4.4", y2: "12", idx: 6 },
  { x1: "6.63", y1: "6.63", x2: "5.07", y2: "5.07", idx: 7 },
];

function readIsDark(): boolean {
  if (typeof document === "undefined") return false;
  return (
    document.documentElement.getAttribute("data-theme")?.toLowerCase() === "dark"
  );
}

export function ThemeToggle({
  id = "btn-theme-toggle",
}: {
  id?: string;
}): React.JSX.Element {
  const [isDark, setIsDark] = useState<boolean>(readIsDark);
  const rawId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const maskId = `sf-theme-mask-${rawId}`;
  const btnRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    setIsDark(readIsDark());
    const handler = (ev: Event): void => {
      const custom = ev as CustomEvent<{ theme?: string }>;
      if (custom.detail?.theme) {
        setIsDark(custom.detail.theme.toLowerCase() === "dark");
      } else {
        setIsDark(readIsDark());
      }
    };
    window.addEventListener("sf-theme-change", handler);
    return () => {
      window.removeEventListener("sf-theme-change", handler);
    };
  }, []);

  const label = isDark ? "切换至浅色纸张主题" : "切换至深色墨黑主题";

  return (
    <button
      id={id}
      ref={btnRef}
      type="button"
      className={`sf-theme-toggle${isDark ? " sf-theme-toggle--dark" : ""}`}
      aria-label={label}
      title={label}
      aria-pressed={isDark}
      onClick={(ev) => {
        if (typeof window.__toggleTheme === "function") {
          window.__toggleTheme(ev.nativeEvent, btnRef.current);
        } else {
          const next = isDark ? "light" : "dark";
          document.documentElement.setAttribute("data-theme", next);
          try {
            window.localStorage.setItem("theme", next);
          } catch {
            // Ignore storage errors
          }
          setIsDark(!isDark);
        }
      }}
    >
      <svg
        className="sf-theme-toggle__svg"
        viewBox="0 0 24 24"
        fill="none"
        aria-hidden="true"
        focusable="false"
      >
        <defs>
          <mask id={maskId}>
            <rect x="0" y="0" width="24" height="24" fill="white" />
            <circle
              className="sf-theme-toggle__cutout"
              cx="24"
              cy="7"
              r="6.5"
              fill="black"
              style={{
                transform: isDark ? "translateX(-7px)" : "translateX(0px)",
              }}
            />
          </mask>
        </defs>
        <circle
          className="sf-theme-toggle__core"
          cx="12"
          cy="12"
          r={isDark ? 8 : 5}
          fill="currentColor"
          mask={`url(#${maskId})`}
        />
        <g stroke="currentColor" strokeWidth="1.75" strokeLinecap="round">
          {SUN_RAYS.map((ray) => (
            <line
              key={ray.idx}
              className="sf-theme-toggle__ray"
              x1={ray.x1}
              y1={ray.y1}
              x2={ray.x2}
              y2={ray.y2}
              style={{ "--ray-idx": ray.idx } as React.CSSProperties}
            />
          ))}
        </g>
      </svg>
    </button>
  );
}
