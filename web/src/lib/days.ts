/**
 * Local calendar days. Everything works on local midnights, so a day follows the viewer's time
 * zone and its DST changes. The gateway keeps usage in quarter-hour buckets, and every UTC offset
 * in use is a whole number of quarter hours, so these bounds split its history exactly.
 */
import type { Locale } from "@/lib/i18n-context";

export const DAY_MS = 86_400_000;

/** Local midnight starting the day that contains `ms`. */
export function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Local midnight `n` days after the day starting at `dayStart`; DST-safe. */
export function addDays(dayStart: number, n: number): number {
  const d = new Date(dayStart);
  d.setDate(d.getDate() + n);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Whole days from the day of `a` to the day of `b`. */
export function daysBetween(a: number, b: number): number {
  return Math.round((startOfDay(b) - startOfDay(a)) / DAY_MS);
}

/** `count + 1` consecutive local midnights starting at `first`: the bounds of `count` days. */
export function dayBounds(first: number, count: number): number[] {
  return Array.from({ length: count + 1 }, (_, i) => addDays(first, i));
}

const pad2 = (n: number): string => String(n).padStart(2, "0");

/** `YYYY-MM-DD` of the local day starting at `dayStart`. */
export function dayKey(dayStart: number): string {
  const d = new Date(dayStart);
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}

/** Local midnight of a `YYYY-MM-DD` key, or `null` when it names no real day. */
export function parseDayKey(key: string): number | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(key);
  if (!match) return null;
  const [year, month, day] = [Number(match[1]), Number(match[2]), Number(match[3])];
  const d = new Date(year, month - 1, day);
  if (d.getFullYear() !== year || d.getMonth() !== month - 1 || d.getDate() !== day) return null;
  return d.getTime();
}

const intlTag = (locale: Locale): string => (locale === "zh" ? "zh-CN" : "en-US");

/** `11月27日周四` / `Thu, Nov 27`. */
export function formatDayLong(dayStart: number, locale: Locale): string {
  return new Intl.DateTimeFormat(intlTag(locale), { month: "short", day: "numeric", weekday: "short" }).format(
    new Date(dayStart),
  );
}

/** `11/27`. */
export function formatDayShort(dayStart: number): string {
  const d = new Date(dayStart);
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

/** `11月27日` / `Nov 27`. */
export function formatDayMonth(dayStart: number, locale: Locale): string {
  return new Intl.DateTimeFormat(intlTag(locale), { month: "short", day: "numeric" }).format(new Date(dayStart));
}
