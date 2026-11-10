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
      tools: [{ name: "list_accounts", description: "Lists the accounts", enabled: true, destructive: false, reversible: false }],
    },
    {
      group: "escape",
      enabled: false,
      tools: [{ name: "graph_post", description: "Free-form call", enabled: false, destructive: true, reversible: false }],
    },
  ],
});

test("changing the write mode calls the command of the chosen platform", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Tools />, {
    get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string),
    set_write_mode: (a: Record<string, unknown>) => {
      calls.push([a.platform, a.mode]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("radio", { name: "Meta Ads" }));
  await userEvent.click(await screen.findByRole("radio", { name: "Read only" }));
  await waitFor(() => expect(calls).toEqual([["meta_ads", "read_only"]]));
});

test("the Escape group appears off, with the tool marked as destructive", async () => {
  renderWithApp(<Tools />, { get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string) });
  expect(await screen.findByRole("switch", { name: "Raw API calls" })).toHaveAttribute("aria-checked", "false");
  await userEvent.click(screen.getByRole("button", { name: /Raw API calls/ }));
  expect(await screen.findByText("graph_post")).toBeInTheDocument();
  expect(screen.getByText("Destructive")).toBeInTheDocument();
});

test("an approval wait outside the range shows an error", async () => {
  renderWithApp(<Tools />, {
    get_tool_settings: (a: Record<string, unknown>) => view(a.platform as string),
    set_approval_wait: () => {
      throw { code: "INVALID_INPUT", message: "Use 30 to 900 seconds.", field: "approval_wait_secs" };
    },
  });
  const input = await screen.findByLabelText("How long the AI waits for approval (seconds)");
  await userEvent.clear(input);
  await userEvent.type(input, "5");
  await userEvent.tab();
  expect(await screen.findByText("Use 30 to 900 seconds.")).toBeInTheDocument();
});
