import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Tools } from "./Tools";
import { renderWithApp } from "../test/render";

const view = (platform: string) => ({
  platform,
  enabled: true,
  write_mode: "approval",
  approval_wait_secs: 120,
  groups: [
    {
      group: "read",
      enabled: true,
      tools: [{ name: "list_accounts", description: "Lista as contas", enabled: true, destructive: false, reversible: false }],
    },
    {
      group: "escape",
      enabled: false,
      tools: [{ name: "graph_post", description: "Chamada livre", enabled: false, destructive: true, reversible: false }],
    },
  ],
});

test("trocar o modo de escrita chama o comando da plataforma escolhida", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Tools />, {
    get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string),
    set_write_mode: (a: Record<string, unknown>) => {
      calls.push([a.platform, a.mode]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("radio", { name: "Meta Ads" }));
  await userEvent.click(await screen.findByRole("radio", { name: "Só leitura" }));
  await waitFor(() => expect(calls).toEqual([["meta_ads", "read_only"]]));
});

test("o grupo Escape aparece desligado e com a ferramenta marcada como destrutiva", async () => {
  renderWithApp(<Tools />, { get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string) });
  expect(await screen.findByRole("switch", { name: "Chamadas livres à API" })).toHaveAttribute("aria-checked", "false");
  await userEvent.click(screen.getByRole("button", { name: /Chamadas livres à API/ }));
  expect(await screen.findByText("graph_post")).toBeInTheDocument();
  expect(screen.getByText("Destrutiva")).toBeInTheDocument();
});

test("espera da aprovação fora do intervalo mostra erro", async () => {
  renderWithApp(<Tools />, {
    get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string),
    set_approval_wait: () => {
      throw { code: "INVALID_INPUT", message: "Use de 30 a 900 segundos.", field: "approval_wait_secs" };
    },
  });
  const input = await screen.findByLabelText("Quanto tempo a IA espera a aprovação (segundos)");
  await userEvent.clear(input);
  await userEvent.type(input, "5");
  await userEvent.tab();
  expect(await screen.findByText("Use de 30 a 900 segundos.")).toBeInTheDocument();
});
