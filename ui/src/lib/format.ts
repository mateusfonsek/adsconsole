import type { Lang } from "../i18n";

const locale = (lang: Lang | string) => (lang === "pt" ? "pt-BR" : "en-US");

export function formatMoney(value: number, currency: string, lang: Lang | string): string {
  return new Intl.NumberFormat(locale(lang), { style: "currency", currency }).format(value);
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 24 * 3600],
  ["month", 30 * 24 * 3600],
  ["day", 24 * 3600],
  ["hour", 3600],
  ["minute", 60],
  ["second", 1],
];

export function formatRelative(date: Date, lang: Lang | string, now: Date = new Date()): string {
  const secs = Math.round((date.getTime() - now.getTime()) / 1000);
  const rtf = new Intl.RelativeTimeFormat(locale(lang), { numeric: "auto" });
  for (const [unit, size] of UNITS) {
    if (Math.abs(secs) >= size || unit === "second") {
      return rtf.format(Math.round(secs / size), unit);
    }
  }
  return "";
}

export function formatDateTime(date: Date, lang: Lang | string): string {
  return new Intl.DateTimeFormat(locale(lang), { dateStyle: "short", timeStyle: "short" }).format(date);
}

/** Long ids keep the start and the end: `act_1234…123456`. */
export function formatId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-6)}` : id;
}

/** Segundos como `m:ss`. */
export function formatCountdown(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}
