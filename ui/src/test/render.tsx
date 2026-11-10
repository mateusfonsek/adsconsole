import type { ReactElement } from "react";
import { render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { I18nextProvider } from "react-i18next";
import { createI18n, type Lang } from "../i18n";
import { installIpcMock, type IpcHandlers } from "./ipc-mock";
import type { ChangeView, PendingView } from "../bindings";

/** Renders with React Query (no retries), i18n and the mocked IPC. */
export function renderWithApp(ui: ReactElement, handlers: IpcHandlers = {}, lang: Lang = "en") {
  installIpcMock(handlers);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const i18n = createI18n(lang);
  const result = render(
    <QueryClientProvider client={qc}>
      <I18nextProvider i18n={i18n}>{ui}</I18nextProvider>
    </QueryClientProvider>,
  );
  return { ...result, qc, i18n };
}

export function changeFixture(id: string, over: Partial<ChangeView> = {}): ChangeView {
  return {
    id,
    platform: "meta_ads",
    account_id: "act_1234567890123456",
    connection_name: "Portfolio",
    tool: "update_adset",
    summary: "Increase ad set budget",
    status: "pending",
    reversible: true,
    created_at: "2026-10-31T12:00:00Z",
    before: { daily_budget: 50 },
    after: { daily_budget: 80 },
    error: null,
    orphaned_ids: [],
    undone_by: null,
    ...over,
  } as ChangeView;
}

export function pendingFixture(id: string, over: Partial<PendingView> = {}): PendingView {
  return {
    change: changeFixture(id),
    warning: null,
    expires_at: "2099-01-01T00:00:00Z",
    ai_waiting_until: "2026-10-31T12:02:00Z",
    blocked_reason: null,
    ...over,
  } as PendingView;
}
