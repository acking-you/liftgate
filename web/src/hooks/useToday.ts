import { useEffect, useState } from "react";
import { addDays, startOfDay } from "@/lib/days";

/** Local midnight of today; rolls over at the next local midnight, even on an idle dashboard. */
export function useToday(): number {
  const [today, setToday] = useState(() => startOfDay(Date.now()));
  useEffect(() => {
    const roll = (): void => setToday(startOfDay(Date.now()));
    // A sleeping laptop fires the timer late, and a hidden tab may throttle it: recheck on focus.
    const timer = window.setTimeout(roll, Math.max(1_000, addDays(today, 1) - Date.now() + 500));
    document.addEventListener("visibilitychange", roll);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", roll);
    };
  }, [today]);
  return today;
}
