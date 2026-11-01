import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { History } from "./History";
import { renderWithApp, changeFixture } from "../test/render";

test("desfazer com conflito mostra a mensagem", async () => {
  renderWithApp(<History />, {
    list_changes: [changeFixture("c1", { status: "applied" })],
    undo: () => {
      throw { code: "UNDO_CONFLICT", message: "O objeto mudou depois desta alteração.", field: null };
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Desfazer" }));
  expect(await screen.findByText("O objeto mudou depois desta alteração.")).toBeInTheDocument();
});

test("irreversível e já desfeita não mostram o botão", async () => {
  renderWithApp(<History />, {
    list_changes: [
      changeFixture("c1", { status: "applied", reversible: false, summary: "Upload de vídeo" }),
      changeFixture("c2", { status: "applied", undone_by: "c9", summary: "Pausar campanha" }),
    ],
  });
  expect(await screen.findByText("Upload de vídeo")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Desfazer" })).not.toBeInTheDocument();
  expect(screen.getByText("Irreversível")).toBeInTheDocument();
  expect(screen.getByText("Desfeita")).toBeInTheDocument();
});

test("o detalhe lista os objetos que sobraram", async () => {
  renderWithApp(<History />, {
    list_changes: [changeFixture("c1", { status: "failed", orphaned_ids: ["c1_obj", "s1_obj"] })],
    get_change: changeFixture("c1", { status: "failed", orphaned_ids: ["c1_obj", "s1_obj"] }),
  });
  await userEvent.click(await screen.findByText("Aumentar orçamento do conjunto"));
  expect(await screen.findByText("c1_obj")).toBeInTheDocument();
  expect(screen.getByText("s1_obj")).toBeInTheDocument();
});

test("filtro de status vai para o comando", async () => {
  const filters: unknown[] = [];
  renderWithApp(<History />, {
    list_changes: (a: Record<string, unknown>) => {
      filters.push((a.filter as { status: string | null }).status);
      return [];
    },
  });
  await userEvent.click(await screen.findByRole("radio", { name: "Falharam" }));
  await waitFor(() => expect(filters).toContain("failed"));
});
