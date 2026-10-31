import { render, screen } from "@testing-library/react";
import { App } from "./App";
import { installIpcMock } from "./test/ipc-mock";

const settings = {
  language: "pt",
  theme: "dark",
  autostart: false,
  port: 7458,
  secret_backend: "keychain",
  data_dir: "/tmp/x",
  version: "0.1.0",
  repo_url: "https://github.com/o/r",
  demo: false,
};

test("mostra o nome, a versão e aplica o tema salvo", async () => {
  installIpcMock({ app_version: "0.1.0", get_app_settings: settings, list_pending: [] });
  render(<App lang="pt" />);
  expect(screen.getByText("AdsConsole")).toBeInTheDocument();
  expect(await screen.findByText("v0.1.0")).toBeInTheDocument();
  await screen.findByRole("heading", { name: "Início" });
  expect(document.documentElement.classList.contains("dark")).toBe(true);
});

test("troca para o inglês quando as configurações pedem", async () => {
  installIpcMock({ app_version: "0.1.0", get_app_settings: { ...settings, language: "en" }, list_pending: [] });
  render(<App lang="pt" />);
  expect(await screen.findByRole("heading", { name: "Home" })).toBeInTheDocument();
});
