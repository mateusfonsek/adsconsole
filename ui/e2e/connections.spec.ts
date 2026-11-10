import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("a web-type Google JSON shows the error and blocks the login", async ({ page }) => {
  await mockTauri(page);
  await page.goto("/#/connections");
  await page.getByRole("button", { name: "Connect Google Ads" }).click();
  await page.getByRole("button", { name: "Choose file" }).click();
  await expect(page.getByRole("alert")).toContainText("Desktop app");
  await expect(page.getByRole("button", { name: "Sign in with Google" })).toBeDisabled();
});
