import type { Page } from "@playwright/test";

export type Scenario = {
  /** Começa sem conexões e sem cliente visto (primeiros passos abertos). */
  fresh?: boolean;
  /** Quantas aprovações pendentes existem no começo. */
  pending?: number;
};

/**
 * Instala um `__TAURI_INTERNALS__` falso antes da página carregar. O estado vive na
 * página: aprovar tira da fila e põe no histórico, desfazer marca como desfeita.
 * `window.__emit(kind, extra)` dispara um evento do app para a UI.
 */
export async function mockTauri(page: Page, scenario: Scenario = {}) {
  await page.addInitScript((sc: Scenario) => {
    type Change = Record<string, unknown> & { id: string; status: string };
    const change = (id: string, summary: string, status = "pending"): Change => ({
      id,
      platform: "meta_ads",
      account_id: "act_100200300400",
      connection_name: "Portfólio",
      tool: "update_adset",
      summary,
      status,
      reversible: true,
      created_at: new Date().toISOString(),
      before: { daily_budget: 50 },
      after: { daily_budget: 80 },
      error: null,
      orphaned_ids: [],
      undone_by: null,
    });
    const state = {
      settings: {
        language: "pt",
        theme: "light",
        autostart: false,
        port: 7458,
        secret_backend: "keychain",
        data_dir: "/dados",
        version: "0.1.0",
        repo_url: "https://github.com/exemplo/adsconsole",
        demo: false,
      },
      connections: sc.fresh
        ? []
        : [
            { id: "g1", platform: "google_ads", name: "Agência", is_default: true, status: { kind: "ok" }, expires_at: null, accounts: 3, detail: "explorer" },
          ],
      clientSeen: !sc.fresh,
      pending: Array.from({ length: sc.pending ?? 0 }, (_, i) => change(`chg_${i + 1}`, `Aumentar orçamento ${i + 1}`)),
      history: [] as Change[],
      listeners: [] as number[],
      callbacks: new Map<number, (v: unknown) => void>(),
      nextId: 1,
    };
    const pendingView = (c: Change) => ({
      change: c,
      warning: null,
      expires_at: new Date(Date.now() + 86_400_000).toISOString(),
      ai_waiting_until: new Date(Date.now() + 60_000).toISOString(),
      blocked_reason: null,
    });
    const fail = (code: string, message: string, field: string | null = null) => {
      throw { code, message, field };
    };
    const handlers: Record<string, (a: Record<string, unknown>) => unknown> = {
      app_version: () => "0.1.0",
      boot_state: () => ({ issue: null, notices: [] }),
      get_app_settings: () => state.settings,
      set_language: (a) => ((state.settings.language = a.language as string), null),
      set_theme: (a) => ((state.settings.theme = a.theme as string), null),
      onboarding_state: () => ({ has_connection: state.connections.length > 0, client_seen: state.clientSeen }),
      list_connections: () => state.connections,
      google_console_links: () => fail("INVALID_INPUT", "Este JSON é de um client do tipo Aplicativo da Web. Crie um client do tipo App para computador.", "client_json"),
      "plugin:dialog|open": () => "/tmp/client.json",
      server_status: () => ({
        state: { kind: "running", port: 7458 },
        google_enabled: true,
        meta_enabled: true,
        google_tools: 28,
        meta_tools: 20,
        google_write_mode: "approval",
        meta_write_mode: "approval",
      }),
      client_snippet_preview: () => ({ text: 'claude mcp add --transport http adsconsole-google http://127.0.0.1:7458/mcp/google-ads --header "Authorization: Bearer ••••"', where_to_paste: "clients.where.terminal" }),
      copy_client_snippet: () => null,
      list_pending: () => state.pending.map(pendingView),
      approve: (a) => {
        const c = state.pending.find((p) => p.id === a.id) ?? fail("CHANGE_NOT_FOUND", "não encontrada");
        state.pending = state.pending.filter((p) => p.id !== a.id);
        const done = { ...(c as Change), status: "applied" };
        state.history.unshift(done);
        return done;
      },
      reject: (a) => {
        const c = state.pending.find((p) => p.id === a.id) as Change;
        state.pending = state.pending.filter((p) => p.id !== a.id);
        const done = { ...c, status: "rejected" };
        state.history.unshift(done);
        return done;
      },
      list_changes: () => state.history,
      get_change: (a) => state.history.find((c) => c.id === a.id),
      undo: (a) => {
        const c = state.history.find((h) => h.id === a.id) as Change;
        c.undone_by = `undo_${c.id}`;
        return { status: "applied", change_id: `undo_${c.id}` };
      },
      "plugin:event|listen": (a) => {
        state.listeners.push(a.handler as number);
        return state.listeners.length;
      },
      "plugin:event|unlisten": () => null,
    };
    // A ponte mínima que o @tauri-apps/api espera.
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
      transformCallback: (cb: (v: unknown) => void) => {
        const id = state.nextId++;
        state.callbacks.set(id, cb);
        return id;
      },
      unregisterCallback: (id: number) => state.callbacks.delete(id),
      invoke: async (cmd: string, args: Record<string, unknown> = {}) => {
        const h = handlers[cmd];
        if (!h) throw new Error(`sem mock: ${cmd}`);
        return h(args);
      },
    };
    (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
    (window as unknown as Record<string, unknown>).__emit = (kind: string, extra: Record<string, unknown> = {}) => {
      if (kind === "first_call_received") state.clientSeen = true;
      for (const id of state.listeners) state.callbacks.get(id)?.({ event: "app-event-msg", id, payload: { kind, ...extra } });
    };
  }, scenario);
}
