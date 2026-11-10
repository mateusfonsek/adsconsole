import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import type { ReactNode } from "react";

/** Side panel on the right (connection wizards). */
export function Sheet({
  open,
  onOpenChange,
  title,
  description,
  children,
  closeLabel,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  title: string;
  description?: string;
  children: ReactNode;
  closeLabel: string;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-fg/10 backdrop-blur-[1px]" />
        <Dialog.Content className="fixed inset-y-0 right-0 flex w-[400px] max-w-full flex-col overflow-y-auto border-l border-line bg-bg p-6 shadow-[0_0_40px_rgba(0,0,0,0.08)] focus:outline-none">
          <div className="mb-4 flex items-start justify-between gap-4">
            <div>
              <Dialog.Title className="text-base font-semibold tracking-tight">{title}</Dialog.Title>
              {description && (
                <Dialog.Description className="mt-1 text-xs text-muted">{description}</Dialog.Description>
              )}
            </div>
            <Dialog.Close aria-label={closeLabel} className="rounded-console p-1 text-muted hover:text-fg">
              <X size={16} />
            </Dialog.Close>
          </div>
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** Centered confirmation dialog. */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  children,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  title: string;
  description: string;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-fg/20" />
        <Dialog.Content className="fixed left-1/2 top-1/2 w-[400px] max-w-[calc(100%-32px)] -translate-x-1/2 -translate-y-1/2 rounded-console border border-line bg-bg p-5 focus:outline-none">
          <Dialog.Title className="text-[15px] font-semibold tracking-tight">{title}</Dialog.Title>
          <Dialog.Description className="mt-1.5 text-[13px] text-muted">{description}</Dialog.Description>
          <div className="mt-5 flex justify-end gap-2">{children}</div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
