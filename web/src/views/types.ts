import type { ViewId } from "@/hooks/useHashRoute";

export interface ViewProps {
  /** Optional selection carried in the URL after the view id. */
  arg: string | null;
  navigate: (view: ViewId, arg?: string | null) => void;
}
