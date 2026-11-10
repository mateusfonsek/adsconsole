import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";

type Handler = (args: Record<string, unknown>) => unknown;
export type IpcHandlers = Record<string, Handler | unknown>;

/**
 * Mocks the app commands. Each key is a command name; the value is the response
 * or a function that receives the arguments. A function that throws becomes a command error.
 * A command without a handler fails with "no mock: <name>", so the test shows what is missing.
 */
export function installIpcMock(handlers: IpcHandlers): void {
  mockIPC(
    (cmd, payload) => {
      if (!(cmd in handlers)) throw new Error(`no mock: ${cmd}`);
      const h = handlers[cmd];
      return typeof h === "function" ? (h as Handler)((payload ?? {}) as Record<string, unknown>) : h;
    },
    { shouldMockEvents: true },
  );
}

export { clearMocks };
