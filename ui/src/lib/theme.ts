import type { Theme } from "../bindings";

/** Theme: `system` follows the system; the others force the `dark` class. */
export function applyTheme(theme: Theme) {
  const dark =
    theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
}
