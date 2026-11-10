import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

// Starts in Portuguese on purpose: this is the language-switch test, so the pt text stays.
test("switching theme and language", async ({ page }) => {
  await mockTauri(page, { language: "pt" });
  await page.goto("/#/settings");
  await expect(page.getByRole("heading", { name: "Configurações" })).toBeVisible();
  await page.getByRole("radio", { name: "Escuro" }).click();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.getByRole("radio", { name: "English" }).click();
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByRole("link", { name: /Connections/ })).toBeVisible();
});
