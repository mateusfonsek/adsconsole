import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Settings } from "./Settings";
import { renderWithApp } from "../test/render";

const server = {
  state: { kind: "running", port: 7458 },
  google_enabled: true,
  meta_enabled: true,
  google_tools: 1,
  meta_tools: 1,
  google_write_mode: "approval",
  meta_write_mode: "approval",
};

const app = {
  language: "en",
  theme: "system",
  autostart: false,
  port: 7458,
  secret_backend: "keychain",
  data_dir: "/Users/x/Library/Application Support/adsconsole",
  version: "0.1.0",
  repo_url: "https://github.com/o/r",
  demo: false,
};

test("changing the language calls set_language and translates the screen", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Settings />, {
    get_app_settings: { ...app, language: "pt" },
    server_status: server,
    set_language: (a: Record<string, unknown>) => {
      calls.push(a.language);
      return null;
    },
  }, "pt");
  expect(await screen.findByRole("heading", { name: "Configurações" })).toBeInTheDocument();
  await userEvent.click(await screen.findByRole("radio", { name: "English" }));
  await waitFor(() => expect(calls).toEqual(["en"]));
  expect(await screen.findByRole("heading", { name: "Settings" })).toBeInTheDocument();
});

test("a port error shows in the field", async () => {
  renderWithApp(<Settings />, {
    get_app_settings: app,
    server_status: server,
    set_port: () => {
      throw { code: "PORT_IN_USE", message: "Port 7459 is already in use.", field: null };
    },
  });
  const input = await screen.findByLabelText("Port");
  await userEvent.clear(input);
  await userEvent.type(input, "7459");
  await userEvent.click(screen.getByRole("button", { name: "Save port" }));
  expect(await screen.findByText("Port 7459 is already in use.")).toBeInTheDocument();
  expect(input).toHaveAttribute("aria-invalid", "true");
});

test("shows the secrets backend and the data folder", async () => {
  renderWithApp(<Settings />, { get_app_settings: app, server_status: server });
  expect(await screen.findByText(/System keychain/)).toBeInTheDocument();
  expect(screen.getByText(app.data_dir)).toBeInTheDocument();
});
