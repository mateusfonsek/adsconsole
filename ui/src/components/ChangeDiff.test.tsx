import { render, screen } from "@testing-library/react";
import { diffRows, ChangeDiff } from "./ChangeDiff";

test("achata objetos aninhados e omite campos iguais", () => {
  const rows = diffRows(
    { budget: { daily: 50 }, name: "a", status: "PAUSED" },
    { budget: { daily: 80 }, name: "a", status: "ACTIVE", bid: 2 },
  );
  expect(rows).toEqual([
    { path: "bid", before: undefined, after: 2 },
    { path: "budget.daily", before: 50, after: 80 },
    { path: "status", before: "PAUSED", after: "ACTIVE" },
  ]);
});

test("sem antes (criação) mostra só o depois", () => {
  expect(diffRows(null, { campaign_id: "c1" })).toEqual([{ path: "campaign_id", before: undefined, after: "c1" }]);
});

test("renderiza a tabela com o valor antigo riscado", () => {
  render(<ChangeDiff before={{ daily_budget: 50 }} after={{ daily_budget: 80 }} labels={{ field: "Campo", before: "Antes", after: "Depois" }} />);
  expect(screen.getByRole("columnheader", { name: "Campo" })).toBeInTheDocument();
  expect(screen.getByText("50")).toHaveClass("line-through");
  expect(screen.getByText("80")).toBeInTheDocument();
});
