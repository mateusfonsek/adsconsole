import { screen, within } from "@testing-library/react";
import { Home } from "./Home";
import { renderWithApp, pendingFixture } from "../test/render";

const running = {
  state: { kind: "running", port: 7458 },
  google_enabled: true,
  meta_enabled: false,
  google_tools: 28,
  meta_tools: 20,
  google_write_mode: "approval",
  meta_write_mode: "read_only",
};

test("mostra os primeiros passos enquanto não terminou", async () => {
  renderWithApp(<Home />, {
    onboarding_state: { has_connection: true, client_seen: false },
    server_status: running,
    list_connections: [],
    list_pending: [],
  });
  const list = await screen.findByRole("list", { name: "Primeiros passos" });
  const items = within(list).getAllByRole("listitem");
  expect(items).toHaveLength(3);
  expect(items[0]).toHaveAttribute("data-done", "true");
  expect(items[1]).toHaveAttribute("data-done", "false");
  expect(within(items[1]).getByRole("link")).toHaveAttribute("href", "#/mcps");
});

test("com tudo pronto mostra o status no lugar dos passos", async () => {
  renderWithApp(<Home />, {
    onboarding_state: { has_connection: true, client_seen: true },
    server_status: running,
    list_connections: [
      {
        id: "m1",
        platform: "meta_ads",
        name: "Portfólio",
        is_default: true,
        status: { kind: "needs_reconnect" },
        expires_at: null,
        accounts: 2,
        detail: null,
      },
    ],
    list_pending: [pendingFixture("c1")],
  });
  expect(await screen.findByText("127.0.0.1:7458")).toBeInTheDocument();
  expect(screen.queryByRole("list", { name: "Primeiros passos" })).not.toBeInTheDocument();
  expect(await screen.findByText("1 conexão precisa reconectar")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: /1 aprovação pendente/ })).toHaveAttribute("href", "#/approvals");
});
