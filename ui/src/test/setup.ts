import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";
import { clearMocks } from "./ipc-mock";

afterEach(() => {
  cleanup();
  clearMocks();
});
