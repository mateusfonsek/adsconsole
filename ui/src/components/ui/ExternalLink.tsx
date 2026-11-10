import type { ReactNode } from "react";
import { ArrowUpRight } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";

/** Link that opens in the system browser (the app window never navigates away). */
export function ExternalLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      onClick={(e) => {
        e.preventDefault();
        void openUrl(href).catch(() => {});
      }}
      className="inline-flex items-center gap-0.5 text-xs font-medium underline decoration-line-strong underline-offset-2 hover:decoration-fg"
    >
      {children}
      <ArrowUpRight size={12} aria-hidden />
    </a>
  );
}
