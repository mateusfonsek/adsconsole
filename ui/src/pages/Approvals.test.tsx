import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Approvals } from "./Approvals";
import { renderWithApp, pendingFixture, changeFixture } from "../test/render";

test("approving calls the command with the selected id", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Approvals />, {
    list_pending: [pendingFixture("c1")],
    approve: (a: Record<string, unknown>) => {
      calls.push(a.id);
      return changeFixture("c1", { status: "applied" });
    },
  });
  expect(await screen.findByRole("heading", { name: "Increase ad set budget" })).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Approve" }));
  await waitFor(() => expect(calls).toEqual(["c1"]));
});

test("read-only blocks approve and keeps reject", async () => {
  renderWithApp(<Approvals />, { list_pending: [pendingFixture("c1", { blocked_reason: "read_only" })] });
  expect(await screen.findByRole("button", { name: "Approve" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Reject" })).toBeEnabled();
  expect(screen.getByText(/This platform is in read-only mode/)).toBeInTheDocument();
});

test("an irreversible change shows the warning", async () => {
  renderWithApp(<Approvals />, {
    list_pending: [pendingFixture("c1", { change: changeFixture("c1", { reversible: false }) })],
  });
  expect(await screen.findByText("This change cannot be undone.")).toBeInTheDocument();
});

test("the detail disables the buttons when the request expires", async () => {
  const soon = new Date(Date.now() + 1200).toISOString();
  renderWithApp(<Approvals />, { list_pending: [pendingFixture("c1", { expires_at: soon })] });
  expect(await screen.findByRole("button", { name: "Approve" })).toBeEnabled();
  expect(await screen.findByText("Expired: the AI got \"not approved\".", {}, { timeout: 3000 })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Approve" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Reject" })).toBeDisabled();
});

test("empty queue shows the empty state", async () => {
  renderWithApp(<Approvals />, { list_pending: [] });
  expect(await screen.findByText("Nothing waiting for approval.")).toBeInTheDocument();
});

test("the open request that leaves the queue stays on screen, without switching to another", async () => {
  let call = 0;
  window.location.hash = "#/approvals/c1";
  const a = pendingFixture("c1", { change: changeFixture("c1", { summary: "Request A" }) });
  const b = pendingFixture("c2", { change: changeFixture("c2", { summary: "Request B" }) });
  const { qc } = renderWithApp(<Approvals />, {
    list_pending: () => (call++ === 0 ? [a, b] : [b]),
    get_change: changeFixture("c1", { summary: "Request A", status: "expired" }),
  });
  expect(await screen.findByRole("heading", { name: "Request A" })).toBeInTheDocument();
  await qc.invalidateQueries({ queryKey: ["list_pending"] });
  await waitFor(() => expect(call).toBeGreaterThan(1));
  expect(await screen.findByText("Expired: the AI got \"not approved\".")).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Request A" })).toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Request B" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Approve" })).not.toBeInTheDocument();
});

test("with no request in the URL, pins the first one in the URL", async () => {
  window.location.hash = "#/approvals";
  renderWithApp(<Approvals />, { list_pending: [pendingFixture("c1"), pendingFixture("c2")] });
  await waitFor(() => expect(window.location.hash).toBe("#/approvals/c1"));
});
