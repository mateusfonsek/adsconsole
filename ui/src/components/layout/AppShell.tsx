import { useCallback, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { AppEvent } from "../../bindings";
import { useAppEvents } from "../../lib/ipc";
import type { RouteName } from "../../lib/route";
import { Button } from "../ui/Button";
import { Sidebar } from "./Sidebar";

type Update = { version: string; url: string };

export function AppShell({
  route,
  version,
  demo,
  children,
  onEvent,
}: {
  route: RouteName;
  version?: string;
  demo?: boolean;
  children: ReactNode;
  onEvent?: (ev: AppEvent) => void;
}) {
  const { t } = useTranslation();
  const [update, setUpdate] = useState<Update | null>(null);
  const handle = useCallback(
    (ev: AppEvent) => {
      if (ev.kind === "update_available") setUpdate({ version: ev.version, url: ev.url });
      onEvent?.(ev);
    },
    [onEvent],
  );
  useAppEvents(handle);

  return (
    <div className="flex h-full bg-bg text-fg">
      <Sidebar route={route} version={version} />
      <div className="flex min-w-0 flex-1 flex-col">
        {demo && (
          <div className="label-mono border-b border-line bg-line/40 px-8 py-1.5 text-fg">{t("app.demo_banner")}</div>
        )}
        <main className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto max-w-[920px] px-8 pb-16 pt-8">{children}</div>
        </main>
      </div>
      {update && (
        <div
          role="status"
          className="fixed bottom-4 right-4 flex items-center gap-3 rounded-console border border-line bg-surface px-4 py-3 text-[13px] shadow-[0_8px_30px_rgba(0,0,0,0.08)]"
        >
          <span>{t("common.update_available", { version: update.version })}</span>
          <Button size="sm" variant="primary" onClick={() => void openUrl(update.url)}>
            {t("common.download")}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setUpdate(null)}>
            {t("common.dismiss")}
          </Button>
        </div>
      )}
    </div>
  );
}
