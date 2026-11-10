import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { History } from "./History";
import { renderWithApp, changeFixture } from "../test/render";

test("undo with a conflict shows the message", async () => {
  renderWithApp(<History />, {
    list_changes: [changeFixture("c1", { status: "applied" })],
    undo: () => {
      throw { code: "UNDO_CONFLICT", message: "The object changed after this change.", field: null };
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Undo" }));
  expect(await screen.findByText("The object changed after this change.")).toBeInTheDocument();
});

test("irreversible and already undone changes do not show the button", async () => {
  renderWithApp(<History />, {
    list_changes: [
      changeFixture("c1", { status: "applied", reversible: false, summary: "Video upload" }),
      changeFixture("c2", { status: "applied", undone_by: "c9", summary: "Pause campaign" }),
    ],
  });
  expect(await screen.findByText("Video upload")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  expect(screen.getByText("Irreversible")).toBeInTheDocument();
  expect(screen.getByText("Undone")).toBeInTheDocument();
});

test("the detail lists the leftover objects", async () => {
  renderWithApp(<History />, {
    list_changes: [changeFixture("c1", { status: "failed", orphaned_ids: ["c1_obj", "s1_obj"] })],
    get_change: changeFixture("c1", { status: "failed", orphaned_ids: ["c1_obj", "s1_obj"] }),
  });
  await userEvent.click(await screen.findByText("Increase ad set budget"));
  expect(await screen.findByText("c1_obj")).toBeInTheDocument();
  expect(screen.getByText("s1_obj")).toBeInTheDocument();
});

test("status filter goes to the command", async () => {
  const filters: unknown[] = [];
  renderWithApp(<History />, {
    list_changes: (a: Record<string, unknown>) => {
      filters.push((a.filter as { status: string | null }).status);
      return [];
    },
  });
  await userEvent.click(await screen.findByRole("radio", { name: "Failed" }));
  await waitFor(() => expect(filters).toContain("failed"));
});
