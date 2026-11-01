import { expect, test } from "@playwright/test";
import { mockTauri } from "./mock";

test("aprovar tira da fila e aparece no histórico", async ({ page }) => {
  await mockTauri(page, { pending: 2 });
  await page.goto("/#/approvals");
  await expect(page.getByLabel("2 aprovações pendentes")).toBeVisible();
  await page.getByRole("button", { name: "Aprovar" }).click();
  await expect(page.getByLabel("1 aprovação pendente")).toBeVisible();
  await page.getByRole("link", { name: /Histórico/ }).click();
  await expect(page.getByText("Aumentar orçamento 1")).toBeVisible();
  await expect(page.getByText("Aplicada", { exact: true })).toBeVisible();
});

test("fila vazia mostra o estado vazio", async ({ page }) => {
  await mockTauri(page, { pending: 0 });
  await page.goto("/#/approvals");
  await expect(page.getByText("Nada esperando aprovação.")).toBeVisible();
});
