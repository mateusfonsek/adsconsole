import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("getting-started steps mark the first call by themselves", async ({ page }) => {
  await mockTauri(page, { fresh: true });
  await page.goto("/");
  const steps = page.getByRole("list", { name: "Getting started" });
  await expect(steps.getByRole("listitem")).toHaveCount(3);
  await expect(steps.getByRole("listitem").nth(2)).toHaveAttribute("data-done", "false");
  await page.evaluate(() => (window as unknown as { __emit: (k: string) => void }).__emit("first_call_received"));
  await expect(steps.getByRole("listitem").nth(2)).toHaveAttribute("data-done", "true");
});
