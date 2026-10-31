import type { Theme } from "../bindings";

/** Tema: `system` segue o sistema; os outros forçam a classe `dark`. */
export function applyTheme(theme: Theme) {
  const dark =
    theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
}
