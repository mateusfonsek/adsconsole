import { useTranslation } from "react-i18next";
import { commands } from "../bindings";
import { navigate } from "../lib/route";
import { useAction } from "../lib/ipc";
import { Button } from "./ui/Button";
import { ConfirmDialog } from "./ui/Sheet";

/** Pedido de saída com aprovações pendentes (vem da barra de menu). */
export function QuitDialog({ pending, onClose }: { pending: number; onClose: () => void }) {
  const { t } = useTranslation();
  const quit = useAction(() => commands.quitApp(true));
  return (
    <ConfirmDialog
      open
      onOpenChange={(v) => !v && onClose()}
      title={t("quit.title")}
      description={t("quit.pending", { count: pending })}
    >
      <Button
        onClick={() => {
          onClose();
          navigate("approvals");
        }}
      >
        {t("quit.review")}
      </Button>
      <Button variant="danger" disabled={quit.isPending} onClick={() => quit.mutate(undefined)}>
        {t("quit.confirm")}
      </Button>
    </ConfirmDialog>
  );
}
