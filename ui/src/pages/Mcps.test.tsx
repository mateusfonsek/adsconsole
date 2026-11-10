import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Mcps } from "./Mcps";
import { renderWithApp } from "../test/render";

const TOKEN = "s3cr3t-t0ken";

const status = {
  state: { kind: "running", port: 7458 },
  google_enabled: true,
  meta_enabled: true,
  google_tools: 28,
  meta_tools: 20,
  google_write_mode: "approval",
  meta_write_mode: "read_only",
};

const preview = (a: Record<string, unknown>) => ({
  text: `claude mcp add --transport http adsconsole-${a.platform === "google_ads" ? "google" : "meta"} url --header "Authorization: Bearer ••••"`,
  where_to_paste: "clients.where.terminal",
});

test("the snippet preview never shows the token", async () => {
  renderWithApp(<Mcps />, { server_status: status, client_snippet_preview: preview, reveal_token: TOKEN });
  expect(await screen.findByText(/adsconsole-google url/)).toBeInTheDocument();
  expect(document.body.textContent).not.toContain(TOKEN);
  await userEvent.click(screen.getByRole("button", { name: "Show token" }));
  expect(await screen.findByText(TOKEN)).toBeInTheDocument();
});

test("copying the snippet calls the command with client and platform", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Mcps />, {
    server_status: status,
    client_snippet_preview: preview,
    copy_client_snippet: (a: Record<string, unknown>) => {
      calls.push([a.client, a.platform]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("tab", { name: "Codex" }));
  await userEvent.click(screen.getByRole("button", { name: "Copy snippet" }));
  await waitFor(() => expect(calls).toEqual([["codex", "google_ads"]]));
  expect(await screen.findByText("Copied with the token")).toBeInTheDocument();
});

test("pausing an MCP calls the command", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Mcps />, {
    server_status: status,
    client_snippet_preview: preview,
    set_server_enabled: (a: Record<string, unknown>) => {
      calls.push([a.platform, a.enabled]);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("switch", { name: "Meta Ads" }));
  await waitFor(() => expect(calls).toEqual([["meta_ads", false]]));
});

test("a server error shows the message and the port field", async () => {
  renderWithApp(<Mcps />, {
    server_status: {
      ...status,
      state: { kind: "error", port: 7458, error: { code: "PORT_IN_USE", message: "Port 7458 is already in use.", field: null } },
    },
    client_snippet_preview: preview,
  });
  expect(await screen.findByText("Port 7458 is already in use.")).toBeInTheDocument();
  expect(screen.getByLabelText("Port")).toHaveValue("7458");
});

test("a port in use on save shows the message", async () => {
  renderWithApp(<Mcps />, {
    server_status: status,
    client_snippet_preview: preview,
    set_port: () => ({ kind: "error", port: 8080, error: { code: "PORT_IN_USE", message: "Port 8080 is already in use.", field: null } }),
  });
  const input = await screen.findByLabelText("Port");
  await userEvent.clear(input);
  await userEvent.type(input, "8080");
  await userEvent.click(screen.getByRole("button", { name: "Save port" }));
  expect(await screen.findByText("Port 8080 is already in use.")).toBeInTheDocument();
});

test("with the server in error, the same port can be retried", async () => {
  renderWithApp(<Mcps />, {
    server_status: {
      ...status,
      state: { kind: "error", port: 7458, error: { code: "PORT_IN_USE", message: "Port 7458 is already in use.", field: null } },
    },
    client_snippet_preview: preview,
  });
  expect(await screen.findByRole("button", { name: "Save port" })).toBeEnabled();
});
