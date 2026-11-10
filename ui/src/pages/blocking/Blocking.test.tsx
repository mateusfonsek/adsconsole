import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../../App";
import { installIpcMock } from "../../test/ipc-mock";
import { renderWithApp } from "../../test/render";
import { QuitDialog } from "../../components/QuitDialog";

test("an unavailable keychain blocks the app", async () => {
  installIpcMock({
    boot_state: { issue: { kind: "keychain_unavailable" }, notices: [] },
    app_version: "0.1.0",
  });
  render(<App lang="en" />);
  expect(await screen.findByRole("heading", { name: "The system keychain did not respond" })).toBeInTheDocument();
  expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
});

test("using a file warns that the token changes and calls the command", async () => {
  const calls: string[] = [];
  installIpcMock({
    boot_state: { issue: { kind: "keychain_unavailable" }, notices: [] },
    app_version: "0.1.0",
    use_file_backend: () => {
      calls.push("file");
      return { issue: { kind: "keychain_unavailable" }, notices: [] };
    },
  });
  render(<App lang="en" />);
  expect(await screen.findByText(/the MCP token changes/)).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Use a file" }));
  await waitFor(() => expect(calls).toEqual(["file"]));
});

test("the config reset notice appears and can be dismissed", async () => {
  installIpcMock({
    boot_state: { issue: null, notices: [{ kind: "config_reset", message: "config.json was invalid (expected value at line 1 column 1); using the defaults. The old file was saved as config.json.corrupt-20261110T120000" }] },
    app_version: "0.1.0",
    get_app_settings: {
      language: "en",
      theme: "light",
      autostart: false,
      port: 7458,
      secret_backend: "keychain",
      data_dir: "/d",
      version: "0.1.0",
      repo_url: "https://github.com/o/r",
      demo: false,
    },
    list_pending: [],
    onboarding_state: { has_connection: true, client_seen: true },
    server_status: {
      state: { kind: "running", port: 7458 },
      google_enabled: true,
      meta_enabled: true,
      google_tools: 1,
      meta_tools: 1,
      google_write_mode: "approval",
      meta_write_mode: "approval",
    },
    list_connections: [],
  });
  render(<App lang="en" />);
  expect(await screen.findByText("config.json was invalid (expected value at line 1 column 1); using the defaults. The old file was saved as config.json.corrupt-20261110T120000")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
  expect(screen.queryByText("config.json was invalid (expected value at line 1 column 1); using the defaults. The old file was saved as config.json.corrupt-20261110T120000")).not.toBeInTheDocument();
});

test("confirming the quit rejects the pending approvals", async () => {
  const calls: unknown[] = [];
  renderWithApp(<QuitDialog pending={2} onClose={() => {}} />, {
    quit_app: (a: Record<string, unknown>) => {
      calls.push(a.rejectPending);
      return { kind: "ready" };
    },
  });
  expect(screen.getByText("2 pending approvals will be rejected. Quit anyway?")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Reject and quit" }));
  await waitFor(() => expect(calls).toEqual([true]));
});
