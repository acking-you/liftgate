import * as React from "react";
import { ChevronRight, PlugZap, Search } from "lucide-react";
import { formatDurationMs, formatUsd } from "@/lib/format";
import { harnessMeta } from "@/lib/harness";
import { useI18n } from "@/lib/i18n";
import { LATENCY_SCALES, type LatencyKind, latencyTone } from "@/lib/latency";
import { providerMeta } from "@/lib/provider";
import { cn } from "@/lib/utils";

export type Tone = "neutral" | "ok" | "warn" | "err" | "accent";

/* ---------- Card ---------- */

export function Card({
  title,
  sub,
  actions,
  flush = false,
  className,
  bodyClassName,
  children,
}: {
  title?: React.ReactNode;
  sub?: React.ReactNode;
  actions?: React.ReactNode;
  /** Edge-to-edge body for tables. */
  flush?: boolean;
  className?: string;
  bodyClassName?: string;
  children: React.ReactNode;
}): React.JSX.Element {
  const hasHead = title !== undefined || actions !== undefined;
  return (
    <section className={cn("lg-card", className)}>
      {hasHead && (
        <header className="lg-card-head">
          <div className="min-w-0">
            {title !== undefined && <h2 className="lg-card-title">{title}</h2>}
            {sub !== undefined && <p className="lg-card-sub">{sub}</p>}
          </div>
          {actions !== undefined && (
            <div className="flex flex-wrap items-center gap-2">{actions}</div>
          )}
        </header>
      )}
      <div className={cn(flush ? "lg-card-flush" : "lg-card-body", bodyClassName)}>{children}</div>
    </section>
  );
}

/* ---------- Stat tile ---------- */

export function Stat({
  label,
  value,
  unit,
  icon,
  foot,
  onClick,
  children,
}: {
  label: string;
  value: React.ReactNode;
  unit?: React.ReactNode;
  icon?: React.ReactNode;
  foot?: React.ReactNode;
  onClick?: () => void;
  children?: React.ReactNode;
}): React.JSX.Element {
  const body = (
    <>
      <div className="lg-stat-label">
        <span>{label}</span>
        {icon !== undefined && <span className="text-fg-4 [&>svg]:h-3.5 [&>svg]:w-3.5">{icon}</span>}
      </div>
      <div className="lg-stat-value">
        {value}
        {unit !== undefined && <span className="lg-stat-unit">{unit}</span>}
      </div>
      {children}
      {foot !== undefined && <div className="lg-stat-foot">{foot}</div>}
    </>
  );
  if (onClick) {
    return (
      <button type="button" className="lg-stat" onClick={onClick}>
        {body}
      </button>
    );
  }
  return <div className="lg-stat">{body}</div>;
}

/* ---------- Labels ---------- */

export function Pill({
  tone = "neutral",
  icon,
  title,
  children,
}: {
  tone?: Tone;
  icon?: React.ReactNode;
  title?: string;
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <span className="lg-pill" data-tone={tone} title={title}>
      {icon}
      {children}
    </span>
  );
}

export function Tag({
  title,
  children,
}: {
  title?: string;
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <span className="lg-tag" title={title}>
      {children}
    </span>
  );
}

export function Dot({ tone = "neutral", live = false }: { tone?: Tone; live?: boolean }): React.JSX.Element {
  return <span className="lg-dot" data-tone={tone} data-live={live} aria-hidden="true" />;
}

/** Harness identity: a fixed-colour swatch beside the name, never colour alone. */
export function HarnessBadge({
  id,
  className,
}: {
  id: string | null | undefined;
  className?: string;
}): React.JSX.Element {
  const meta = harnessMeta(id);
  return (
    <span className={cn("inline-flex items-center gap-1.5 whitespace-nowrap", className)}>
      <span className="lg-swatch" style={{ background: meta.color }} aria-hidden="true" />
      <span>{meta.label}</span>
    </span>
  );
}

/** Upstream provider identity + optional Passthrough observe-only marker. */
export function ProviderBadge({
  id,
  mode,
  compact = false,
  className,
}: {
  id: string | null | undefined;
  mode?: string | null;
  compact?: boolean;
  className?: string;
}): React.JSX.Element {
  const meta = providerMeta(id);
  const isPassthrough = mode === "passthrough";
  return (
    <span className={cn("inline-flex items-center gap-1.5 whitespace-nowrap", className)}>
      <span className="lg-swatch" style={{ background: meta.color }} aria-hidden="true" />
      <span className="lg-num">{compact ? meta.short : meta.label}</span>
      {isPassthrough && <Pill tone="warn">passthrough</Pill>}
    </span>
  );
}

/* ---------- Controls ---------- */

export function Button({
  variant,
  size,
  icon = false,
  className,
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "ghost" | "danger";
  size?: "sm";
  /** Icon-only square button; pair with `aria-label`. */
  icon?: boolean;
}): React.JSX.Element {
  return (
    <button
      type="button"
      className={cn("lg-btn", className)}
      data-variant={variant}
      data-size={size}
      data-icon={icon ? "true" : undefined}
      {...props}
    />
  );
}

export interface SegOption<T extends string> {
  value: T;
  label: React.ReactNode;
  count?: number;
}

export function Segmented<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
}: {
  value: T;
  onChange: (value: T) => void;
  options: ReadonlyArray<SegOption<T>>;
  ariaLabel: string;
}): React.JSX.Element {
  return (
    <div className="lg-seg" role="group" aria-label={ariaLabel}>
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          className="lg-seg-btn"
          aria-pressed={opt.value === value}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
          {opt.count !== undefined && <span className="lg-seg-count">{opt.count}</span>}
        </button>
      ))}
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  disabled?: boolean;
  label: string;
}): React.JSX.Element {
  return (
    <button
      type="button"
      role="switch"
      className="lg-switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    />
  );
}

export function SearchInput({
  value,
  onChange,
  placeholder,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  className?: string;
}): React.JSX.Element {
  return (
    <div className={cn("relative min-w-0", className)}>
      <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-fg-4" />
      <input
        type="search"
        className="lg-input pl-8"
        placeholder={placeholder}
        aria-label={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
    </div>
  );
}

export function Select({
  value,
  onChange,
  options,
  ariaLabel,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  options: ReadonlyArray<{ value: string; label: string }>;
  ariaLabel: string;
  className?: string;
}): React.JSX.Element {
  return (
    <select
      className={cn("lg-input", className)}
      aria-label={ariaLabel}
      value={value}
      onChange={(e) => onChange(e.target.value)}
    >
      {options.map((opt) => (
        <option key={opt.value} value={opt.value}>
          {opt.label}
        </option>
      ))}
    </select>
  );
}

/* ---------- Meters ---------- */

export function Meter({
  ratio,
  tone,
  label,
}: {
  /** 0..1 */
  ratio: number;
  tone?: "ok" | "warn" | "err";
  label: string;
}): React.JSX.Element {
  const pct = Math.max(0, Math.min(100, ratio * 100));
  return (
    <div
      className="lg-meter"
      data-tone={tone}
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(pct)}
    >
      <span style={{ width: `${pct}%` }} />
    </div>
  );
}

/* ---------- Feedback ---------- */

export function EmptyState({
  icon,
  title,
  hint,
  action,
}: {
  icon?: React.ReactNode;
  title: string;
  hint?: React.ReactNode;
  action?: React.ReactNode;
}): React.JSX.Element {
  return (
    <div className="lg-empty">
      {icon !== undefined && <div className="lg-empty-icon [&>svg]:h-4 [&>svg]:w-4">{icon}</div>}
      <p className="lg-empty-title">{title}</p>
      {hint !== undefined && <p className="lg-empty-hint">{hint}</p>}
      {action !== undefined && <div className="mt-2">{action}</div>}
    </div>
  );
}

export function Note({
  tone = "neutral",
  icon,
  children,
}: {
  tone?: Tone;
  icon?: React.ReactNode;
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <div className="lg-note" data-tone={tone} role={tone === "err" ? "alert" : undefined}>
      {icon}
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

/** Shown where a feature needs gateway endpoints this build does not serve yet. */
export function Unsupported({
  feature,
  endpoint,
  children,
}: {
  feature: string;
  endpoint: string;
  children?: React.ReactNode;
}): React.JSX.Element {
  const { m } = useI18n();
  return (
    <EmptyState
      icon={<PlugZap />}
      title={m.common.unsupportedTitle(feature)}
      hint={
        <>
          {m.common.unsupportedHint(<code className="lg-num text-fg-2">{endpoint}</code>)}
          {children}
        </>
      }
    />
  );
}

export function Loading({ label }: { label?: string }): React.JSX.Element {
  const { m } = useI18n();
  return <div className="px-4 py-10 text-center text-[12.5px] text-fg-3">{label ?? m.common.loading}</div>;
}

/* ---------- Layout helpers ---------- */

export function KV({
  items,
  mono = true,
  className,
}: {
  items: ReadonlyArray<readonly [label: string, value: React.ReactNode]>;
  mono?: boolean;
  className?: string;
}): React.JSX.Element {
  return (
    <dl className={cn("lg-kv", className)}>
      {items.map(([label, value]) => (
        <React.Fragment key={label}>
          <dt>{label}</dt>
          <dd className={mono ? "lg-num text-[12px]" : undefined}>{value}</dd>
        </React.Fragment>
      ))}
    </dl>
  );
}

/** A compact labelled figure for inspector headers. */
export function Figure({
  label,
  value,
  sub,
}: {
  label: string;
  value: React.ReactNode;
  sub?: React.ReactNode;
}): React.JSX.Element {
  return (
    <div className="min-w-0">
      <div className="text-[12px] text-fg-3">{label}</div>
      <div className="mt-0.5 truncate text-[15px] font-semibold text-fg">{value}</div>
      {sub !== undefined && <div className="mt-0.5 truncate text-[11.5px] text-fg-3">{sub}</div>}
    </div>
  );
}

export function Pager({
  page,
  pages,
  total,
  onPage,
}: {
  page: number;
  pages: number;
  total: number;
  onPage: (page: number) => void;
}): React.JSX.Element | null {
  const { m } = useI18n();
  if (pages <= 1) return null;
  return (
    <div className="lg-card-foot">
      <span className="lg-num">{m.common.pager(page, pages, total.toLocaleString("en-US"))}</span>
      <div className="flex gap-1.5">
        <Button size="sm" disabled={page <= 1} onClick={() => onPage(page - 1)}>
          {m.common.prevPage}
        </Button>
        <Button size="sm" disabled={page >= pages} onClick={() => onPage(page + 1)}>
          {m.common.nextPage}
        </Button>
      </div>
    </div>
  );
}

/* ---------- Tabs ---------- */

export interface TabOption<T extends string> {
  value: T;
  label: React.ReactNode;
  count?: number;
}

export function Tabs<T extends string>({
  value,
  onChange,
  tabs,
  ariaLabel,
}: {
  value: T;
  onChange: (value: T) => void;
  tabs: ReadonlyArray<TabOption<T>>;
  ariaLabel: string;
}): React.JSX.Element {
  return (
    <div className="lg-tabs" role="tablist" aria-label={ariaLabel}>
      {tabs.map((tab) => (
        <button
          key={tab.value}
          type="button"
          role="tab"
          className="lg-tab"
          aria-selected={tab.value === value}
          onClick={() => onChange(tab.value)}
        >
          {tab.label}
          {tab.count !== undefined && <span className="lg-seg-count">{tab.count}</span>}
        </button>
      ))}
    </div>
  );
}

/* ---------- Stat grid ---------- */

/** Stat tiles separated by hairlines; the last row stretches so no cell is left blank. */
export function StatGrid({ children }: { children: React.ReactNode }): React.JSX.Element {
  return (
    <section className="lg-statgrid">
      <div className="lg-statgrid-inner">{children}</div>
    </section>
  );
}

/* ---------- Disclosure ---------- */

/** Collapsible block whose body is only mounted while open, so long histories stay cheap. */
export function Disclosure({
  head,
  meta,
  tone,
  defaultOpen = false,
  children,
}: {
  head: React.ReactNode;
  meta?: React.ReactNode;
  tone?: "err";
  defaultOpen?: boolean;
  children: React.ReactNode;
}): React.JSX.Element {
  const [open, setOpen] = React.useState(defaultOpen);
  React.useEffect(() => {
    if (defaultOpen) setOpen(true);
  }, [defaultOpen]);
  return (
    <div className="lg-part" data-tone={tone}>
      <button
        type="button"
        className="lg-part-head"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <ChevronRight
          className="transition-transform duration-150"
          style={open ? { transform: "rotate(90deg)" } : undefined}
        />
        <span className="flex min-w-0 flex-1 items-center gap-2">{head}</span>
        {meta !== undefined && <span className="lg-num flex-none text-[11.5px] text-fg-3">{meta}</span>}
      </button>
      {open && <div className="lg-part-body">{children}</div>}
    </div>
  );
}

/* ---------- Two-step confirm ---------- */

/** Destructive actions ask once more in place instead of opening a dialog. */
export function ConfirmButton({
  label,
  confirmLabel,
  icon,
  disabled,
  onConfirm,
}: {
  label: string;
  confirmLabel: string;
  icon?: React.ReactNode;
  disabled?: boolean;
  onConfirm: () => void;
}): React.JSX.Element {
  const { m } = useI18n();
  const [armed, setArmed] = React.useState(false);

  React.useEffect(() => {
    if (!armed) return;
    const timer = window.setTimeout(() => setArmed(false), 5000);
    return () => window.clearTimeout(timer);
  }, [armed]);

  if (!armed) {
    return (
      <Button size="sm" variant="danger" disabled={disabled} onClick={() => setArmed(true)}>
        {icon}
        {label}
      </Button>
    );
  }
  return (
    <span className="inline-flex items-center gap-1.5">
      <Button
        size="sm"
        variant="danger"
        aria-pressed="false"
        onClick={() => {
          setArmed(false);
          onConfirm();
        }}
      >
        {confirmLabel}
      </Button>
      <Button size="sm" variant="ghost" onClick={() => setArmed(false)}>
        {m.common.cancel}
      </Button>
    </span>
  );
}

/* ---------- Form field ---------- */

export function Field({
  label,
  hint,
  htmlFor,
  children,
}: {
  label: string;
  hint?: React.ReactNode;
  htmlFor: string;
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <div className="min-w-0">
      <label className="lg-label" htmlFor={htmlFor}>
        {label}
      </label>
      {children}
      {hint !== undefined && <p className="lg-hint m-0 mt-1.5">{hint}</p>}
    </div>
  );
}

/** A labelled on/off row: the label explains, the switch acts. */
export function SwitchRow({
  title,
  hint,
  checked,
  disabled,
  onChange,
}: {
  title: string;
  hint?: React.ReactNode;
  checked: boolean;
  disabled?: boolean;
  onChange: (next: boolean) => void;
}): React.JSX.Element {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <div className="text-[12.5px] font-medium text-fg">{title}</div>
        {hint !== undefined && <p className="lg-hint m-0 mt-0.5">{hint}</p>}
      </div>
      <Switch checked={checked} disabled={disabled} onChange={onChange} label={title} />
    </div>
  );
}

/** Hit-rate figure coloured against the low-cache threshold. */
export function HitRate({
  bps,
  thresholdBps,
  digits = 1,
}: {
  bps: number | null | undefined;
  thresholdBps: number;
  digits?: number;
}): React.JSX.Element {
  if (typeof bps !== "number") return <span className="text-fg-4">—</span>;
  return (
    <span className={cn("lg-num", bps < thresholdBps ? "font-medium text-err" : "text-fg")}>
      {(bps / 100).toFixed(digits)}%
    </span>
  );
}

/**
 * A latency figure with a traffic-light dot: green when fast, plain when normal, amber when
 * slow, red when very slow. The tooltip names the band and the scale.
 */
export function Latency({
  ms,
  kind,
  pending = false,
}: {
  ms: number | null | undefined;
  kind: LatencyKind;
  /** Still waiting: show the live figure without judging it yet. */
  pending?: boolean;
}): React.JSX.Element {
  const { m } = useI18n();
  if (typeof ms !== "number" || !Number.isFinite(ms)) return <span className="text-fg-4">—</span>;
  const scale = LATENCY_SCALES[kind];
  const tone = pending ? "neutral" : latencyTone(ms, scale);
  const label = m.common.latency[kind];
  const title = `${m.common.latency.hint(label, formatDurationMs(ms), m.common.latency.levels[tone])}\n${m.common.latency.scale(
    [formatDurationMs(scale.ok), formatDurationMs(scale.neutral), formatDurationMs(scale.warn)],
  )}`;
  return (
    <span className="lg-lat lg-num" data-tone={tone} title={title}>
      {formatDurationMs(ms)}
    </span>
  );
}

/** A cost in US dollars, or a muted "unpriced" mark when the model has no price. */
export function Usd({
  value,
  unpricedHint,
}: {
  value: number | null | undefined;
  unpricedHint?: boolean;
}): React.JSX.Element {
  const { m } = useI18n();
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return unpricedHint ? (
      <span className="text-[11.5px] text-warn">{m.common.cost.unpriced}</span>
    ) : (
      <span className="text-fg-4">—</span>
    );
  }
  return <span className="lg-num">{formatUsd(value)}</span>;
}
