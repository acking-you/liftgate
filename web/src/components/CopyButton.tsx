import * as React from "react";
import { useState } from "react";
import { Check, Copy } from "lucide-react";

export function CopyButton({
  value,
  label,
  title = "复制",
}: {
  value: string | (() => string);
  /** When set the button shows text; otherwise it is an icon-only control. */
  label?: string;
  title?: string;
}): React.JSX.Element {
  const [copied, setCopied] = useState(false);

  const handleCopy = (ev: React.MouseEvent): void => {
    ev.stopPropagation();
    const text = typeof value === "function" ? value() : value;
    navigator.clipboard.writeText(text).then(
      () => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1500);
      },
      () => {
        // Clipboard is unavailable outside secure contexts; nothing to recover.
      },
    );
  };

  return (
    <button
      type="button"
      className="lg-btn"
      data-size="sm"
      data-variant={label ? undefined : "ghost"}
      data-icon={label ? undefined : "true"}
      title={label ? undefined : title}
      aria-label={label ? undefined : title}
      onClick={handleCopy}
    >
      {copied ? <Check className="text-ok" /> : <Copy />}
      {label && <span>{copied ? "已复制" : label}</span>}
    </button>
  );
}
