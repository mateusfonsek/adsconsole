import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Settings } from "./Settings";
import { renderWithApp } from "../test/render";

const app = {
  language: "pt",
  theme: "system",
  autostart: false,
  port: 7458,
  secret_backend: "keychain",
  data_dir: "/Users/x/Library/Application Support/adsconsole",
  version: "0.1.0",
  repo_url: "https://github.com/o/r",
  demo: false,
};

test("trocar o idioma chama set_language e traduz a tela", async () => {
  const calls: unknown[] = [];
  renderWithApp(<Settings />, {
    get_app_settings: app,
    set_language: (a: Record<string, unknown>) => {
      calls.push(a.language);
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("radio", { name: "English" }));
  await waitFor(() => expect(calls).toEqual(["en"]));
  expect(await screen.findByRole("heading", { name: "Settings" })).toBeInTheDocument();
});

test("erro de porta aparece no campo", async () => {
  renderWithApp(<Settings />, {
    get_app_settings: app,
    set_port: () => {
      throw { code: "PORT_IN_USE", message: "A porta 7459 já está em uso.", field: null };
    },
  });
  const input = await screen.findByLabelText("Porta");
  await userEvent.clear(input);
  await userEvent.type(input, "7459");
  await userEvent.click(screen.getByRole("button", { name: "Salvar porta" }));
  expect(await screen.findByText("A porta 7459 já está em uso.")).toBeInTheDocument();
  expect(input).toHaveAttribute("aria-invalid", "true");
});

test("mostra o backend de segredos e a pasta de dados", async () => {
  renderWithApp(<Settings />, { get_app_settings: app });
  expect(await screen.findByText(/Keychain do sistema/)).toBeInTheDocument();
  expect(screen.getByText(app.data_dir)).toBeInTheDocument();
});
