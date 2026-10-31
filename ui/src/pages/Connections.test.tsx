import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Connections } from "./Connections";
import { renderWithApp } from "../test/render";

const settings = {
  language: "pt",
  theme: "system",
  autostart: false,
  port: 7458,
  secret_backend: "keychain",
  data_dir: "/d",
  version: "0.1.0",
  repo_url: "https://github.com/o/r",
  demo: false,
};

const conns = [
  {
    id: "g1",
    platform: "google_ads",
    name: "Agência",
    is_default: true,
    status: { kind: "ok" },
    expires_at: null,
    accounts: 4,
    detail: "explorer",
  },
  {
    id: "m1",
    platform: "meta_ads",
    name: "Portfólio",
    is_default: false,
    status: { kind: "expiring_soon", days: 3 },
    expires_at: null,
    accounts: 2,
    detail: "login",
  },
  {
    id: "m2",
    platform: "meta_ads",
    name: "Antiga",
    is_default: false,
    status: { kind: "needs_reconnect" },
    expires_at: null,
    accounts: 0,
    detail: "login",
  },
];

test("lista agrupada por plataforma com o status", async () => {
  renderWithApp(<Connections />, { list_connections: conns, get_app_settings: settings });
  const google = await screen.findByRole("region", { name: "Google Ads" });
  expect(await within(google).findByText("Agência")).toBeInTheDocument();
  const meta = screen.getByRole("region", { name: "Meta Ads" });
  expect(within(meta).getByText("Vence em 3 dias")).toBeInTheDocument();
  expect(within(meta).getByRole("button", { name: "Reconectar Antiga" })).toBeInTheDocument();
});

test("JSON do Google inválido aparece na área do arquivo", async () => {
  renderWithApp(<Connections />, {
    list_connections: [],
    get_app_settings: settings,
    "plugin:dialog|open": "/tmp/client.json",
    google_console_links: () => {
      throw {
        code: "INVALID_INPUT",
        message: "Este JSON é de um client do tipo Aplicativo da Web. Crie um client do tipo App para computador.",
        field: "client_json",
      };
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Conectar Google Ads" }));
  await userEvent.click(await screen.findByRole("button", { name: "Escolher arquivo" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("App para computador");
  expect(screen.getByRole("button", { name: "Entrar com Google" })).toBeDisabled();
});

test("cancelar o login da Meta mantém o formulário preenchido", async () => {
  let reject: ((e: unknown) => void) | undefined;
  renderWithApp(<Connections />, {
    list_connections: [],
    get_app_settings: settings,
    add_meta_login: () =>
      new Promise((_, rej) => {
        reject = rej;
      }),
    cancel_login: () => {
      reject?.({ code: "LOGIN_CANCELLED", message: "Login cancelado.", field: null });
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Conectar Meta Ads" }));
  await userEvent.type(screen.getByLabelText("Nome da conexão"), "Minha");
  await userEvent.type(screen.getByLabelText("App ID"), "123");
  await userEvent.type(screen.getByLabelText("Config ID"), "456");
  await userEvent.type(screen.getByLabelText("App Secret"), "segredo");
  await userEvent.click(screen.getByRole("button", { name: "Entrar com Facebook" }));
  expect(await screen.findByText("Aguardando o login no navegador…")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Cancelar login" }));
  await waitFor(() => expect(screen.getByLabelText("App ID")).toHaveValue("123"));
  expect(screen.getByLabelText("Nome da conexão")).toHaveValue("Minha");
  expect(screen.getByLabelText("Config ID")).toHaveValue("456");
  expect(screen.getByLabelText("App Secret")).toHaveValue("");
});

test("links da Meta que dependem do App ID só aparecem com ele", async () => {
  renderWithApp(<Connections />, { list_connections: [], get_app_settings: settings });
  await userEvent.click(await screen.findByRole("button", { name: "Conectar Meta Ads" }));
  expect(screen.queryByRole("link", { name: /Configurações básicas/ })).not.toBeInTheDocument();
  await userEvent.type(screen.getByLabelText("App ID"), "999");
  expect(screen.getByRole("link", { name: /Configurações básicas/ })).toHaveAttribute(
    "href",
    "https://developers.facebook.com/apps/999/settings/basic/",
  );
  expect(screen.getByRole("link", { name: /Modelos/ })).toHaveAttribute(
    "href",
    "https://github.com/o/r/tree/main/docs/templates",
  );
});
