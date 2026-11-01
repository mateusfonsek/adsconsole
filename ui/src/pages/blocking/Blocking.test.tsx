import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../../App";
import { installIpcMock } from "../../test/ipc-mock";
import { renderWithApp } from "../../test/render";
import { QuitDialog } from "../../components/QuitDialog";

test("keychain indisponível bloqueia o app", async () => {
  installIpcMock({
    boot_state: { issue: { kind: "keychain_unavailable" }, notices: [] },
    app_version: "0.1.0",
  });
  render(<App lang="pt" />);
  expect(await screen.findByRole("heading", { name: "O keychain do sistema não respondeu" })).toBeInTheDocument();
  expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
});

test("usar arquivo avisa que o token muda e chama o comando", async () => {
  const calls: string[] = [];
  installIpcMock({
    boot_state: { issue: { kind: "keychain_unavailable" }, notices: [] },
    app_version: "0.1.0",
    use_file_backend: () => {
      calls.push("file");
      return { issue: { kind: "keychain_unavailable" }, notices: [] };
    },
  });
  render(<App lang="pt" />);
  expect(await screen.findByText(/o token do MCP muda/)).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Usar arquivo" }));
  await waitFor(() => expect(calls).toEqual(["file"]));
});

test("aviso de config restaurada aparece e pode ser dispensado", async () => {
  installIpcMock({
    boot_state: { issue: null, notices: [{ kind: "config_reset", message: "config.json estava inválido." }] },
    app_version: "0.1.0",
    get_app_settings: {
      language: "pt",
      theme: "light",
      autostart: false,
      port: 7458,
      secret_backend: "keychain",
      data_dir: "/d",
      version: "0.1.0",
      repo_url: "https://github.com/o/r",
      demo: false,
    },
    list_pending: [],
    onboarding_state: { has_connection: true, client_seen: true },
    server_status: {
      state: { kind: "running", port: 7458 },
      google_enabled: true,
      meta_enabled: true,
      google_tools: 1,
      meta_tools: 1,
      google_write_mode: "approval",
      meta_write_mode: "approval",
    },
    list_connections: [],
  });
  render(<App lang="pt" />);
  expect(await screen.findByText("config.json estava inválido.")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Dispensar" }));
  expect(screen.queryByText("config.json estava inválido.")).not.toBeInTheDocument();
});

test("confirmar a saída recusa as pendências", async () => {
  const calls: unknown[] = [];
  renderWithApp(<QuitDialog pending={2} onClose={() => {}} />, {
    quit_app: (a: Record<string, unknown>) => {
      calls.push(a.rejectPending);
      return { kind: "ready" };
    },
  });
  expect(screen.getByText("2 aprovações pendentes serão recusadas. Sair mesmo assim?")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Recusar e sair" }));
  await waitFor(() => expect(calls).toEqual([true]));
});
