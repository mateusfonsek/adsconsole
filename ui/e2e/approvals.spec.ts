import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("approving removes it from the queue and shows it in history", async ({ page }) => {
  await mockTauri(page, { pending: 2 });
  await page.goto("/#/approvals");
  await expect(page.getByLabel("2 pending approvals")).toBeVisible();
  await page.getByRole("button", { name: "Approve" }).click();
  await expect(page.getByLabel("1 pending approval")).toBeVisible();
  await page.getByRole("link", { name: /History/ }).click();
  await expect(page.getByText("Increase budget 1")).toBeVisible();
  await expect(page.getByRole("table").getByText("Applied", { exact: true })).toBeVisible();
});

test("empty queue shows the empty state", async ({ page }) => {
  await mockTauri(page, { pending: 0 });
  await page.goto("/#/approvals");
  await expect(page.getByText("Nothing waiting for approval.")).toBeVisible();
});
