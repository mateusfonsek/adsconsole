import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "../../components/ui/Button";

/** Numbered step of a wizard, with optional links. */
export function WizardStep({ n, title, children }: { n: number; title: string; children?: ReactNode }) {
  return (
    <li className="flex gap-3 border-b border-line py-3 last:border-b-0">
      <span aria-hidden className="mt-px grid h-[18px] w-[18px] shrink-0 place-items-center rounded-[4px] border border-line-strong font-mono text-[10px] text-muted">
        {n}
      </span>
      <div className="flex min-w-0 flex-col gap-1">
        <span className="text-[13px]">{title}</span>
        {children && <div className="flex flex-wrap gap-x-3 gap-y-1">{children}</div>}
      </div>
    </li>
  );
}

/** Browser login state, with the cancel button. */
export function WaitingLogin({ onCancel }: { onCancel: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-start gap-3 rounded-console border border-line bg-surface p-4">
      <span className="inline-flex items-center gap-2 text-[13px] font-medium">
        <span aria-hidden className="h-[7px] w-[7px] animate-pulse bg-fg" />
        {t("conn.waiting")}
      </span>
      <span className="text-xs text-muted">{t("conn.waiting_hint")}</span>
      <Button size="sm" onClick={onCancel}>
        {t("conn.cancel_login")}
      </Button>
    </div>
  );
}
