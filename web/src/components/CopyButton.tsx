import * as React from "react";
import { useState } from "react";
import { Check, Copy } from "lucide-react";

export function CopyButton({
  value,
  label = "复制",
  compact = false,
  id,
}: {
  value: string;
  label?: string;
  compact?: boolean;
  id?: string;
}): React.JSX.Element {
  const [copied, setCopied] = useState(false);

  const handleCopy = async (ev: React.MouseEvent): Promise<void> => {
    ev.stopPropagation();
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      // Clipboard API unavailable in non-secure context
    }
  };

  if (compact) {
    return (
      <button
        id={id}
        type="button"
        className="sf-btn sf-btn--ghost sf-btn--sm px-1.5 h-6 text-[11px]"
        title={label}
        onClick={(ev) => {
          void handleCopy(ev);
        }}
      >
        {copied ? (
          <Check className="h-3 w-3 text-[var(--ok)]" />
        ) : (
          <Copy className="h-3 w-3" />
        )}
      </button>
    );
  }

  return (
    <button
      id={id}
      type="button"
      className="sf-btn sf-btn--sm"
      onClick={(ev) => {
        void handleCopy(ev);
      }}
    >
      {copied ? (
        <>
          <Check className="h-3.5 w-3.5 text-[var(--ok)]" />
          <span className="text-[var(--ok)]">已复制</span>
        </>
      ) : (
        <>
          <Copy className="h-3.5 w-3.5" />
          <span>{label}</span>
        </>
      )}
    </button>
  );
}
