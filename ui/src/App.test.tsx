import { render, screen } from "@testing-library/react";
import { App } from "./App";
import { installIpcMock } from "./test/ipc-mock";

const home = {
  boot_state: { issue: null, notices: [] },
  onboarding_state: { has_connection: false, client_seen: false },
  server_status: { state: { kind: "running", port: 7458 }, google_enabled: true, meta_enabled: true, google_tools: 1, meta_tools: 1, google_write_mode: "approval", meta_write_mode: "approval" },
  list_connections: [],
};

const settings = {
  language: "en",
  theme: "dark",
  autostart: false,
  port: 7458,
  secret_backend: "keychain",
  data_dir: "/tmp/x",
  version: "0.1.0",
  repo_url: "https://github.com/o/r",
  demo: false,
};

test("shows the name, the version and applies the saved theme", async () => {
  installIpcMock({ app_version: "0.1.0", get_app_settings: settings, list_pending: [], ...home });
  render(<App lang="en" />);
  expect(await screen.findByText("AdsConsole")).toBeInTheDocument();
  expect(await screen.findByText("v0.1.0")).toBeInTheDocument();
  await screen.findByRole("heading", { name: "Home" });
  expect(document.documentElement.classList.contains("dark")).toBe(true);
});

test("switches to English when the settings ask for it", async () => {
  installIpcMock({ app_version: "0.1.0", get_app_settings: { ...settings, language: "en" }, list_pending: [], ...home });
  render(<App lang="pt" />);
  expect(await screen.findByRole("heading", { name: "Home" })).toBeInTheDocument();
});
