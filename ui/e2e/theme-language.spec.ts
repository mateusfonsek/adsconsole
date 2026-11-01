import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("trocar tema e idioma", async ({ page }) => {
  await mockTauri(page);
  await page.goto("/#/settings");
  await page.getByRole("radio", { name: "Escuro" }).click();
  await expect(page.locator("html")).toHaveClass(/dark/);
  await page.getByRole("radio", { name: "English" }).click();
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await expect(page.getByRole("link", { name: /Connections/ })).toBeVisible();
});
