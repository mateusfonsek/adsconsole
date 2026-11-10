import type { ReactNode } from "react";

export type Tone = "ok" | "warn" | "err" | "muted";

const square: Record<Tone, string> = {
  ok: "bg-ok",
  warn: "bg-warn",
  err: "bg-err",
  muted: "bg-muted",
};

const text: Record<Tone, string> = {
  ok: "text-ok",
  warn: "text-warn",
  err: "text-err",
  muted: "text-muted",
};

/** Indicador quadrado de 7 px, a assinatura de status do visual. */
export function StatusSquare({ tone }: { tone: Tone }) {
  return <span aria-hidden className={`inline-block h-[7px] w-[7px] shrink-0 ${square[tone]}`} />;
}

/** Status label: square + text in uppercase mono. */
export function StatusLabel({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <span className={`label-mono inline-flex items-center gap-1.5 ${text[tone]}`}>
      <StatusSquare tone={tone} />
      {children}
    </span>
  );
}

/** Mono counter in a pill (e.g. approvals). */
export function CountBadge({ count, label }: { count: number; label: string }) {
  return (
    <span
      aria-label={label}
      className="min-w-[18px] rounded-[4px] bg-fg px-1 text-center font-mono text-[10.5px] leading-[16px] text-bg"
    >
      {count > 99 ? "99+" : count}
    </span>
  );
}
