import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient, type QueryKey } from "@tanstack/react-query";
import { events, type AppEvent } from "../bindings";

/** Erro que os comandos devolvem (o `AppError` do Rust). */
export type AppErr = { code: string; message: string; field: string | null; details?: unknown };

export function asAppError(e: unknown): AppErr {
  if (e && typeof e === "object" && "code" in e && "message" in e) return e as AppErr;
  return { code: "UNKNOWN", message: e instanceof Error ? e.message : String(e), field: null };
}

/** Consulta um comando; a chave é o nome do comando mais os argumentos. */
export function useCmd<T>(key: QueryKey, fn: () => Promise<T>, enabled = true) {
  return useQuery<T, AppErr>({
    queryKey: key,
    enabled,
    queryFn: async () => {
      try {
        return await fn();
      } catch (e) {
        throw asAppError(e);
      }
    },
  });
}

/** Executa um comando que muda estado e invalida as chaves dadas no fim. */
export function useAction<A, T>(fn: (args: A) => Promise<T>, invalidate: string[] = []) {
  const qc = useQueryClient();
  return useMutation<T, AppErr, A>({
    mutationFn: async (args) => {
      try {
        return await fn(args);
      } catch (e) {
        throw asAppError(e);
      }
    },
    onSettled: () => Promise.all(invalidate.map((k) => qc.invalidateQueries({ queryKey: [k] }))),
  });
}

/** Chaves invalidadas por cada evento do app. */
export function keysFor(ev: AppEvent): string[] {
  switch (ev.kind) {
    case "approval_requested":
    case "change_updated":
      return ["list_pending", "list_changes", "get_change"];
    case "server_status_changed":
      return ["server_status", "get_tool_settings"];
    case "connection_status_changed":
      return ["list_connections", "onboarding_state"];
    case "first_call_received":
      return ["onboarding_state"];
    default:
      return [];
  }
}

/** Ouve os eventos do app: invalida o cache e repassa o evento a `onEvent`. */
export function useAppEvents(onEvent?: (ev: AppEvent) => void) {
  const qc = useQueryClient();
  useEffect(() => {
    let stop: (() => void) | undefined;
    let cancelled = false;
    events.appEventMsg
      .listen((e) => {
        const ev = e.payload as AppEvent;
        for (const key of keysFor(ev)) void qc.invalidateQueries({ queryKey: [key] });
        onEvent?.(ev);
      })
      .then((un) => {
        if (cancelled) un();
        else stop = un;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      stop?.();
    };
  }, [qc, onEvent]);
}
