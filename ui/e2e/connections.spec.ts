import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("JSON do Google do tipo web mostra o erro e bloqueia o login", async ({ page }) => {
  await mockTauri(page);
  await page.goto("/#/connections");
  await page.getByRole("button", { name: "Conectar Google Ads" }).click();
  await page.getByRole("button", { name: "Escolher arquivo" }).click();
  await expect(page.getByRole("alert")).toContainText("App para computador");
  await expect(page.getByRole("button", { name: "Entrar com Google" })).toBeDisabled();
});
