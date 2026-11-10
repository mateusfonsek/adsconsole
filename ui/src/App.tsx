import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { QueryClient, QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { I18nextProvider, useTranslation } from "react-i18next";
import { commands, type AppEvent } from "./bindings";
import { QuitDialog } from "./components/QuitDialog";
import { ConfigNotice, KeychainError } from "./pages/blocking/KeychainError";
import { applyTheme } from "./lib/theme";
import { Approvals } from "./pages/Approvals";
import { Connections } from "./pages/Connections";
import { History } from "./pages/History";
import { Home } from "./pages/Home";
import { Mcps } from "./pages/Mcps";
import { Tools } from "./pages/Tools";
import { Settings } from "./pages/Settings";
import { createI18n, systemLang, type Lang } from "./i18n";
import { useCmd } from "./lib/ipc";
import { useRoute, type RouteName } from "./lib/route";
import { AppShell } from "./components/layout/AppShell";

const PAGES: Record<RouteName, () => ReactNode> = {
  home: () => <Home />,
  connections: () => <Connections />,
  mcps: () => <Mcps />,
  tools: () => <Tools />,
  approvals: () => <Approvals />,
  history: () => <History />,
  settings: () => <Settings />,
};

function Root({ notices }: { notices: string[] }) {
  const route = useRoute();
  const { i18n } = useTranslation();
  const settings = useCmd(["get_app_settings"], () => commands.getAppSettings());
  const version = useCmd(["app_version"], () => commands.appVersion());
  const [dismissed, setDismissed] = useState(false);
  const [quitPending, setQuitPending] = useState<number | null>(null);
  const onEvent = useCallback((ev: AppEvent) => {
    if (ev.kind === "quit_requested") setQuitPending(ev.pending);
  }, []);

  useEffect(() => {
    const s = settings.data;
    if (!s) return;
    applyTheme(s.theme);
    if (i18n.language !== s.language) void i18n.changeLanguage(s.language);
    document.documentElement.lang = s.language;
  }, [settings.data, i18n]);

  return (
    <AppShell route={route.name} version={version.data} demo={settings.data?.demo} onEvent={onEvent}>
      {!dismissed && notices.length > 0 && <ConfigNotice message={notices.join(" ")} onDismiss={() => setDismissed(true)} />}
      {PAGES[route.name]()}
      {quitPending !== null && <QuitDialog pending={quitPending} onClose={() => setQuitPending(null)} />}
    </AppShell>
  );
}

/** Queries the boot state first: with the keychain down, the app shows only the error screen. */
function Gate() {
  const qc = useQueryClient();
  const boot = useCmd(["boot_state"], () => commands.bootState());
  if (!boot.data) return null;
  if (boot.data.issue && boot.data.issue.kind !== "config_reset") {
    return <KeychainError issue={boot.data.issue} onBoot={(b) => qc.setQueryData(["boot_state"], b)} />;
  }
  const notices = boot.data.notices.flatMap((n) => (n.kind === "config_reset" ? [n.message] : []));
  return <Root notices={notices} />;
}

export function App({ lang }: { lang?: Lang }) {
  const qc = useMemo(() => new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 5_000 } } }), []);
  const i18n = useMemo(() => createI18n(lang ?? systemLang()), [lang]);
  return (
    <QueryClientProvider client={qc}>
      <I18nextProvider i18n={i18n}>
        <Gate />
      </I18nextProvider>
    </QueryClientProvider>
  );
}
