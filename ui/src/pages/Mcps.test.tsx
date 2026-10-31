import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Mcps } from "./Mcps";
import { renderWithApp } from "../test/render";

const TOKEN = "s3cr3t-t0ken";

const status = {
  state: { kind: "running", port: 7458 },
  google_enabled: true,
  meta_enabled: true,
  google_tools: 28,
  meta_tools: 20,
  google_write_mode: "approval",
  meta_write_mode: "read_only",
};

const preview = (a: Record<string, unknown>) => ({
  text: `claude mcp add --transport http adsconsole-${a.platform === "google_ads" ? "google" : "meta"} url --header "Authorization: Bearer ••••"`,
  where_to_paste: "clients.where.terminal",
});

test("a prévia do trecho nunca mostra o token", async () => {
  renderWithApp(<Mcps />, { server_status: status, client_snippet_preview: preview, reveal_token: TOKEN });
  expect(await screen.findByText(/adsconsole-google url/)).toBeInTheDocument();
  expect(document.body.textContent).not.toContain(TOKEN);
  await userEvent.click(screen.getByRole("button", { name: "Mostrar token" }));
  expect(await screen.findByText(TOKEN)).toBeInTheDocument();
});

test("copiar o trecho chama o comando com cliente e plataforma", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Mcps />, {
    server_status: status,
    client_snippet_preview: preview,
    copy_client_snippet: (a: Record<string, unknown>) => {
      calls.push([a.client, a.platform]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("tab", { name: "Codex" }));
  await userEvent.click(screen.getByRole("button", { name: "Copiar trecho" }));
  await waitFor(() => expect(calls).toEqual([["codex", "google_ads"]]));
  expect(await screen.findByText("Copiado com o token")).toBeInTheDocument();
});

test("pausar um MCP chama o comando", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Mcps />, {
    server_status: status,
    client_snippet_preview: preview,
    set_server_enabled: (a: Record<string, unknown>) => {
      calls.push([a.platform, a.enabled]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("switch", { name: "Meta Ads" }));
  await waitFor(() => expect(calls).toEqual([["meta_ads", false]]));
});

test("erro do servidor mostra a mensagem e o campo de porta", async () => {
  renderWithApp(<Mcps />, {
    server_status: {
      ...status,
      state: { kind: "error", port: 7458, error: { code: "PORT_IN_USE", message: "A porta 7458 já está em uso.", field: null } },
    },
    client_snippet_preview: preview,
  });
  expect(await screen.findByText("A porta 7458 já está em uso.")).toBeInTheDocument();
  expect(screen.getByLabelText("Porta")).toHaveValue("7458");
});
