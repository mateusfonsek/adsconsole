import { screen } from "@testing-library/react";
import { Sidebar } from "./Sidebar";
import { renderWithApp, pendingFixture } from "../../test/render";

test("sidebar mostra os itens e o contador de pendências", async () => {
  renderWithApp(<Sidebar route="home" />, {
    list_pending: [pendingFixture("c1"), pendingFixture("c2")],
  });
  expect(screen.getByText("Início")).toBeInTheDocument();
  expect(screen.getByText("Conexões")).toBeInTheDocument();
  expect(screen.getByText("Configurações")).toBeInTheDocument();
  expect(await screen.findByLabelText("2 aprovações pendentes")).toHaveTextContent("2");
});

test("item ativo fica marcado", () => {
  renderWithApp(<Sidebar route="mcps" />, { list_pending: [] });
  expect(screen.getByRole("link", { name: /MCPs/ })).toHaveAttribute("aria-current", "page");
});
