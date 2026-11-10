import { screen } from "@testing-library/react";
import { Sidebar } from "./Sidebar";
import { renderWithApp, pendingFixture } from "../../test/render";

test("sidebar shows the items and the pending counter", async () => {
  renderWithApp(<Sidebar route="home" />, {
    list_pending: [pendingFixture("c1"), pendingFixture("c2")],
  });
  expect(screen.getByText("Home")).toBeInTheDocument();
  expect(screen.getByText("Connections")).toBeInTheDocument();
  expect(screen.getByText("Settings")).toBeInTheDocument();
  expect(await screen.findByLabelText("2 pending approvals")).toHaveTextContent("2");
});

test("active item is marked", () => {
  renderWithApp(<Sidebar route="mcps" />, { list_pending: [] });
  expect(screen.getByRole("link", { name: /MCPs/ })).toHaveAttribute("aria-current", "page");
});
