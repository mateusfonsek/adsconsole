import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Approvals } from "./Approvals";
import { renderWithApp, pendingFixture, changeFixture } from "../test/render";

test("aprovar chama o comando com o id selecionado", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Approvals />, {
    list_pending: [pendingFixture("c1")],
    approve: (a: Record<string, unknown>) => {
      calls.push(a.id);
      return changeFixture("c1", { status: "applied" });
    },
  });
  expect(await screen.findByRole("heading", { name: "Aumentar orçamento do conjunto" })).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Aprovar" }));
  await waitFor(() => expect(calls).toEqual(["c1"]));
});

test("só leitura bloqueia o aprovar e mantém o recusar", async () => {
  renderWithApp(<Approvals />, { list_pending: [pendingFixture("c1", { blocked_reason: "read_only" })] });
  expect(await screen.findByRole("button", { name: "Aprovar" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Recusar" })).toBeEnabled();
  expect(screen.getByText(/O modo atual desta plataforma é só leitura/)).toBeInTheDocument();
});

test("mudança irreversível mostra o aviso", async () => {
  renderWithApp(<Approvals />, {
    list_pending: [pendingFixture("c1", { change: changeFixture("c1", { reversible: false }) })],
  });
  expect(await screen.findByText("Esta mudança não pode ser desfeita.")).toBeInTheDocument();
});

test("o detalhe desativa os botões quando o pedido expira", async () => {
  const soon = new Date(Date.now() + 1200).toISOString();
  renderWithApp(<Approvals />, { list_pending: [pendingFixture("c1", { expires_at: soon })] });
  expect(await screen.findByRole("button", { name: "Aprovar" })).toBeEnabled();
  expect(await screen.findByText("Expirou: a IA recebeu \"não aprovado\".", {}, { timeout: 3000 })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Aprovar" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Recusar" })).toBeDisabled();
});

test("fila vazia mostra o estado vazio", async () => {
  renderWithApp(<Approvals />, { list_pending: [] });
  expect(await screen.findByText("Nada esperando aprovação.")).toBeInTheDocument();
});
