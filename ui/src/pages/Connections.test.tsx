import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Connections } from "./Connections";
import { renderWithApp } from "../test/render";

const settings = {
  language: "en",
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
    name: "Agency",
    is_default: true,
    status: { kind: "ok" },
    expires_at: null,
    accounts: 4,
    detail: "explorer",
  },
  {
    id: "m1",
    platform: "meta_ads",
    name: "Portfolio",
    is_default: false,
    status: { kind: "expiring_soon", days: 3 },
    expires_at: null,
    accounts: 2,
    detail: "login",
  },
  {
    id: "m2",
    platform: "meta_ads",
    name: "Old",
    is_default: false,
    status: { kind: "needs_reconnect" },
    expires_at: null,
    accounts: 0,
    detail: "login",
  },
];

test("list grouped by platform with the status", async () => {
  renderWithApp(<Connections />, { list_connections: conns, get_app_settings: settings });
  const google = await screen.findByRole("region", { name: "Google Ads" });
  expect(await within(google).findByText("Agency")).toBeInTheDocument();
  const meta = screen.getByRole("region", { name: "Meta Ads" });
  expect(within(meta).getByText("Expires in 3 days")).toBeInTheDocument();
  expect(within(meta).getByRole("button", { name: "Reconnect Old" })).toBeInTheDocument();
});

test("an invalid Google JSON shows in the file area", async () => {
  renderWithApp(<Connections />, {
    list_connections: [],
    get_app_settings: settings,
    "plugin:dialog|open": "/tmp/client.json",
    google_console_links: () => {
      throw {
        code: "INVALID_INPUT",
        message: "This JSON is for a Web application client. Create a Desktop app client instead.",
        field: "client_json",
      };
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Connect Google Ads" }));
  await userEvent.click(await screen.findByRole("button", { name: "Choose file" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Desktop app");
  expect(screen.getByRole("button", { name: "Sign in with Google" })).toBeDisabled();
});

test("cancelling the Meta login keeps the form filled in", async () => {
  let reject: ((e: unknown) => void) | undefined;
  renderWithApp(<Connections />, {
    list_connections: [],
    get_app_settings: settings,
    add_meta_login: () =>
      new Promise((_, rej) => {
        reject = rej;
      }),
    cancel_login: () => {
      reject?.({ code: "LOGIN_CANCELLED", message: "Login cancelled.", field: null });
      return null;
    },
  });
  await userEvent.click(await screen.findByRole("button", { name: "Connect Meta Ads" }));
  await userEvent.type(screen.getByLabelText("Connection name"), "Mine");
  await userEvent.type(screen.getByLabelText("App ID"), "123");
  await userEvent.type(screen.getByLabelText("Config ID"), "456");
  await userEvent.type(screen.getByLabelText("App Secret"), "secret");
  await userEvent.click(screen.getByRole("button", { name: "Sign in with Facebook" }));
  expect(await screen.findByText("Waiting for the browser login…")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Cancel login" }));
  await waitFor(() => expect(screen.getByLabelText("App ID")).toHaveValue("123"));
  expect(screen.getByLabelText("Connection name")).toHaveValue("Mine");
  expect(screen.getByLabelText("Config ID")).toHaveValue("456");
  expect(screen.getByLabelText("App Secret")).toHaveValue("");
});

test("Meta links that depend on the App ID only appear with it", async () => {
  renderWithApp(<Connections />, { list_connections: [], get_app_settings: settings });
  await userEvent.click(await screen.findByRole("button", { name: "Connect Meta Ads" }));
  expect(screen.queryByRole("link", { name: /Basic settings/ })).not.toBeInTheDocument();
  await userEvent.type(screen.getByLabelText("App ID"), "999");
  expect(screen.getByRole("link", { name: /Basic settings/ })).toHaveAttribute(
    "href",
    "https://developers.facebook.com/apps/999/settings/basic/",
  );
  expect(screen.getByRole("link", { name: /Privacy policy and redirect page templates/ })).toHaveAttribute(
    "href",
    "https://github.com/o/r/tree/main/docs/templates",
  );
});
