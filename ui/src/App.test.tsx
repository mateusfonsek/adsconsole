import { render, screen } from "@testing-library/react";
import { App } from "./App";
import { installIpcMock } from "./test/ipc-mock";

test("mostra o nome e a versão do app", async () => {
  installIpcMock({ app_version: "0.1.0" });
  render(<App />);
  expect(screen.getByText("AdsConsole")).toBeInTheDocument();
  expect(await screen.findByText("0.1.0")).toBeInTheDocument();
});
