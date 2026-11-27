import * as React from "react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { useI18n } from "@/lib/i18n";
import { cn } from "@/lib/utils";

/** Land focus on the panel itself, so opening with the mouse does not ring the close button. */
function focusPanel(ev: Event): void {
  ev.preventDefault();
  if (ev.currentTarget instanceof HTMLElement) ev.currentTarget.focus();
}

function CloseButton(): React.JSX.Element {
  const { m } = useI18n();
  return (
    <DialogPrimitive.Close className="lg-btn shrink-0" data-variant="ghost" data-icon="true" aria-label={m.common.close}>
      <X />
    </DialogPrimitive.Close>
  );
}

/** Centered dialog for short, focused tasks (connection settings, live probe). */
export function Modal({
  open,
  onOpenChange,
  title,
  description,
  className,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: React.ReactNode;
  description?: React.ReactNode;
  className?: string;
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="lg-overlay" />
        <DialogPrimitive.Content
          className={cn("lg-dialog", className)}
          onOpenAutoFocus={focusPanel}
          {...(description ? {} : { "aria-describedby": undefined })}
        >
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <DialogPrimitive.Title className="m-0 text-[15px] font-semibold text-fg">
                {title}
              </DialogPrimitive.Title>
              {description && (
                <DialogPrimitive.Description className="m-0 mt-1 text-[12.5px] text-fg-3">
                  {description}
                </DialogPrimitive.Description>
              )}
            </div>
            <CloseButton />
          </div>
          {children}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

/** Right-hand inspector: detail on demand without losing the list behind it. */
export function Sheet({
  open,
  onOpenChange,
  title,
  description,
  size = "default",
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: React.ReactNode;
  description?: React.ReactNode;
  size?: "default" | "wide";
  children: React.ReactNode;
}): React.JSX.Element {
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="lg-overlay" />
        <DialogPrimitive.Content
          className="lg-sheet"
          data-size={size}
          onOpenAutoFocus={focusPanel}
          {...(description ? {} : { "aria-describedby": undefined })}
        >
          <div className="lg-sheet-head">
            <div className="min-w-0 flex-1">
              <DialogPrimitive.Title asChild>
                <div className="flex flex-wrap items-center gap-2 text-[15px] font-semibold text-fg">
                  {title}
                </div>
              </DialogPrimitive.Title>
              {description && (
                <DialogPrimitive.Description asChild>
                  <div className="mt-1 text-[12.5px] text-fg-3">{description}</div>
                </DialogPrimitive.Description>
              )}
            </div>
            <CloseButton />
          </div>
          <div className="lg-sheet-body">{children}</div>
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
