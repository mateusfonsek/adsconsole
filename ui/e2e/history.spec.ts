import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("desfazer uma mudança aplicada", async ({ page }) => {
  await mockTauri(page, { pending: 1 });
  await page.goto("/#/approvals");
  await page.getByRole("button", { name: "Aprovar" }).click();
  await page.goto("/#/history");
  await page.getByRole("button", { name: "Desfazer" }).click();
  await expect(page.getByText("Desfeita.")).toBeVisible();
});
