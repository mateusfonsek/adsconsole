import { mockIPC, clearMocks } from "@tauri-apps/api/mocks";

type Handler = (args: Record<string, unknown>) => unknown;
export type IpcHandlers = Record<string, Handler | unknown>;

/**
 * Simula os comandos do app. Cada chave é o nome do comando; o valor é a resposta
 * ou uma função que recebe os argumentos. Uma função que lança vira erro do comando.
 * Comando sem handler falha com "sem mock: <nome>", para o teste mostrar o que falta.
 */
export function installIpcMock(handlers: IpcHandlers): void {
  mockIPC(
    (cmd, payload) => {
      if (!(cmd in handlers)) throw new Error(`sem mock: ${cmd}`);
      const h = handlers[cmd];
      return typeof h === "function" ? (h as Handler)((payload ?? {}) as Record<string, unknown>) : h;
    },
    { shouldMockEvents: true },
  );
}

export { clearMocks };
