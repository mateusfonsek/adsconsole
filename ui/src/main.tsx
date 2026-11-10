import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles/globals.css";

// The system theme applies until the app settings load.
const dark = window.matchMedia("(prefers-color-scheme: dark)");
const applySystemTheme = () => document.documentElement.classList.toggle("dark", dark.matches);
applySystemTheme();
dark.addEventListener("change", applySystemTheme);

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
