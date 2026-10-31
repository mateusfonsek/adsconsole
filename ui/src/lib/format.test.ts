import { formatMoney, formatId, formatRelative } from "./format";

test("formatMoney BRL em pt", () => {
  expect(formatMoney(80, "BRL", "pt")).toBe("R$ 80,00");
});

test("formatMoney USD em en", () => {
  expect(formatMoney(1234.5, "USD", "en")).toBe("$1,234.50");
});

test("formatRelative mostra minutos atrás", () => {
  const now = new Date("2026-10-31T12:00:00Z");
  expect(formatRelative(new Date("2026-10-31T11:58:00Z"), "pt", now)).toBe("há 2 minutos");
  expect(formatRelative(new Date("2026-10-31T11:58:00Z"), "en", now)).toBe("2 minutes ago");
});

test("formatId encurta ids longos no meio", () => {
  expect(formatId("act_1234567890123456")).toBe("act_1234…123456");
  expect(formatId("123-456")).toBe("123-456");
});
