import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("undoing an applied change", async ({ page }) => {
  await mockTauri(page, { pending: 1 });
  await page.goto("/#/approvals");
  await page.getByRole("button", { name: "Approve" }).click();
  await page.goto("/#/history");
  await page.getByRole("button", { name: "Undo" }).click();
  await expect(page.getByText("Undone.")).toBeVisible();
});
