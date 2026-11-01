import { useTranslation } from "react-i18next";
import { KeyRound } from "lucide-react";
import { commands, type BootStateView } from "../../bindings";
import { useAction } from "../../lib/ipc";
import { Button } from "../../components/ui/Button";
import { ErrorNote } from "../../components/ui/Card";

/** Tela que substitui o app quando o boot falhou (keychain ou erro de E/S). */
export function KeychainError({ issue, onBoot }: { issue: BootStateView["issue"]; onBoot: (b: BootStateView) => void }) {
  const { t } = useTranslation();
  const retry = useAction(() => commands.retryKeychain());
  const useFile = useAction(() => commands.useFileBackend());
  const fatal = issue?.kind === "fatal";

  return (
    <main className="grid h-full place-items-center bg-bg px-6 text-fg">
      <div className="flex max-w-[440px] flex-col gap-4">
        <KeyRound size={22} strokeWidth={1.5} aria-hidden className="text-muted" />
        <h1 className="text-[20px] font-semibold tracking-tight">{fatal ? t("boot.fatal_title") : t("boot.keychain_title")}</h1>
        <p className="text-[13px] text-muted">{fatal ? t("boot.fatal_desc") : t("boot.keychain_desc")}</p>
        {fatal && issue?.kind === "fatal" && <ErrorNote message={issue.message} />}
        {!fatal && (
          <>
            <p className="text-[13px]">{t("boot.keychain_retry_hint")}</p>
            <div className="rounded-console border border-warn/40 bg-warn/10 px-3 py-2 text-xs">{t("boot.use_file_warning")}</div>
          </>
        )}
        <div className="flex gap-2">
          <Button variant="primary" disabled={retry.isPending} onClick={() => retry.mutate(undefined, { onSuccess: onBoot })}>
            {t("common.retry")}
          </Button>
          {!fatal && (
            <Button disabled={useFile.isPending} onClick={() => useFile.mutate(undefined, { onSuccess: onBoot })}>
              {t("boot.use_file")}
            </Button>
          )}
        </div>
        {(retry.error || useFile.error) && <ErrorNote message={(retry.error ?? useFile.error)!.message} />}
      </div>
    </main>
  );
}

/** Faixa dispensável com avisos do boot (config restaurada). */
export function ConfigNotice({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  const { t } = useTranslation();
  return (
    <div role="status" className="mb-6 flex items-start justify-between gap-4 rounded-console border border-warn/40 bg-warn/10 px-4 py-3 text-[13px]">
      <span>{message}</span>
      <Button size="sm" variant="ghost" onClick={onDismiss}>
        {t("common.dismiss")}
      </Button>
    </div>
  );
}
