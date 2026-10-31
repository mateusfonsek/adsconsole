import { useEffect, useMemo, type ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { I18nextProvider, useTranslation } from "react-i18next";
import { commands } from "./bindings";
import { applyTheme } from "./lib/theme";
import { Connections } from "./pages/Connections";
import { Home } from "./pages/Home";
import { Mcps } from "./pages/Mcps";
import { Tools } from "./pages/Tools";
import { Settings } from "./pages/Settings";
import { createI18n, systemLang, type Lang } from "./i18n";
import { useCmd } from "./lib/ipc";
import { useRoute, type RouteName } from "./lib/route";
import { AppShell } from "./components/layout/AppShell";
import { PageHeader } from "./components/layout/PageHeader";

function Placeholder({ name }: { name: RouteName }) {
  const { t } = useTranslation();
  return <PageHeader title={t(`nav.${name}`)} />;
}

const PAGES: Record<RouteName, () => ReactNode> = {
  home: () => <Home />,
  connections: () => <Connections />,
  mcps: () => <Mcps />,
  tools: () => <Tools />,
  approvals: () => <Placeholder name="approvals" />,
  history: () => <Placeholder name="history" />,
  settings: () => <Settings />,
};

function Root() {
  const route = useRoute();
  const { i18n } = useTranslation();
  const settings = useCmd(["get_app_settings"], () => commands.getAppSettings());
  const version = useCmd(["app_version"], () => commands.appVersion());

  useEffect(() => {
    const s = settings.data;
    if (!s) return;
    applyTheme(s.theme);
    if (i18n.language !== s.language) void i18n.changeLanguage(s.language);
    document.documentElement.lang = s.language;
  }, [settings.data, i18n]);

  return (
    <AppShell route={route.name} version={version.data} demo={settings.data?.demo}>
      {PAGES[route.name]()}
    </AppShell>
  );
}

export function App({ lang }: { lang?: Lang }) {
  const qc = useMemo(() => new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 5_000 } } }), []);
  const i18n = useMemo(() => createI18n(lang ?? systemLang()), [lang]);
  return (
    <QueryClientProvider client={qc}>
      <I18nextProvider i18n={i18n}>
        <Root />
      </I18nextProvider>
    </QueryClientProvider>
  );
}
