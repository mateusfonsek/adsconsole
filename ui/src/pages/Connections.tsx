import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Plus } from "lucide-react";
import { commands, type ConnectionView, type Platform } from "../bindings";
import { useAction, useCmd } from "../lib/ipc";
import { PageHeader } from "../components/layout/PageHeader";
import { Button } from "../components/ui/Button";
import { Card, ErrorNote } from "../components/ui/Card";
import { ConfirmDialog, Sheet } from "../components/ui/Sheet";
import { StatusLabel } from "../components/ui/Status";
import { GoogleWizard } from "./connections/GoogleWizard";
import { MetaWizard } from "./connections/MetaWizard";

const PLATFORMS: Platform[] = ["google_ads", "meta_ads"];

function Status({ c }: { c: ConnectionView }) {
  const { t } = useTranslation();
  switch (c.status.kind) {
    case "needs_reconnect":
      return <StatusLabel tone="err">{t("conn.status_reconnect")}</StatusLabel>;
    case "expiring_soon":
      return <StatusLabel tone="warn">{t("conn.status_expiring", { count: c.status.days })}</StatusLabel>;
    default:
      return <StatusLabel tone="ok">{t("conn.status_ok")}</StatusLabel>;
  }
}

function Row({ c }: { c: ConnectionView }) {
  const { t } = useTranslation();
  const [confirm, setConfirm] = useState(false);
  const [tested, setTested] = useState<number | null>(null);
  const inv = ["list_connections", "onboarding_state"];
  const reconnect = useAction(() => commands.reconnectConnection(c.id), inv);
  const test = useAction(() => commands.testConnection(c.id), inv);
  const makeDefault = useAction(() => commands.setDefaultConnection(c.id), inv);
  const remove = useAction(() => commands.removeConnection(c.id), inv);
  const error = [reconnect.error, test.error, makeDefault.error, remove.error].find(
    (e) => e && e.code !== "LOGIN_CANCELLED",
  );

  return (
    <div className="border-b border-line px-5 py-3.5 last:border-b-0">
      <div className="flex items-center gap-4">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-[13px] font-medium">{c.name}</span>
            {c.is_default && <span className="label-mono">{t("conn.default")}</span>}
          </div>
          <div className="mt-0.5 flex items-center gap-2 text-xs text-muted">
            <span>{t("conn.accounts", { count: c.accounts })}</span>
            {c.detail && <span className="font-mono text-[11px]">· {c.detail}</span>}
            {tested !== null && <span>· {t("conn.test_ok", { count: tested })}</span>}
          </div>
        </div>
        <Status c={c} />
        <div className="flex items-center gap-1">
          {c.status.kind !== "ok" ? (
            <Button
              size="sm"
              variant="primary"
              aria-label={t("conn.reconnect_named", { name: c.name })}
              disabled={reconnect.isPending}
              onClick={() => reconnect.mutate(undefined)}
            >
              {t("conn.reconnect")}
            </Button>
          ) : (
            <Button size="sm" variant="ghost" disabled={test.isPending} onClick={() => test.mutate(undefined, { onSuccess: setTested })}>
              {t("conn.test")}
            </Button>
          )}
          {!c.is_default && (
            <Button size="sm" variant="ghost" onClick={() => makeDefault.mutate(undefined)}>
              {t("conn.make_default")}
            </Button>
          )}
          <Button size="sm" variant="ghost" aria-label={t("conn.remove_named", { name: c.name })} onClick={() => setConfirm(true)}>
            {t("conn.remove")}
          </Button>
        </div>
      </div>
      {reconnect.isPending && <p className="mt-2 text-xs text-muted">{t("conn.waiting")}</p>}
      {error && (
        <div className="mt-2">
          <ErrorNote message={error.message} code={error.code} />
        </div>
      )}
      <ConfirmDialog
        open={confirm}
        onOpenChange={setConfirm}
        title={t("conn.remove_title", { name: c.name })}
        description={t("conn.remove_desc")}
      >
        <Button onClick={() => setConfirm(false)}>{t("common.cancel")}</Button>
        <Button variant="danger" onClick={() => remove.mutate(undefined, { onSuccess: () => setConfirm(false) })}>
          {t("conn.remove")}
        </Button>
      </ConfirmDialog>
    </div>
  );
}

export function Connections() {
  const { t } = useTranslation();
  const conns = useCmd(["list_connections"], () => commands.listConnections());
  const settings = useCmd(["get_app_settings"], () => commands.getAppSettings());
  const [wizard, setWizard] = useState<Platform | null>(null);
  const templatesUrl = `${settings.data?.repo_url ?? "https://github.com"}/tree/main/docs/templates`;

  return (
    <>
      <PageHeader title={t("conn.title")} subtitle={t("conn.subtitle")} />
      {conns.error && <ErrorNote message={conns.error.message} code={conns.error.code} />}
      {PLATFORMS.map((p) => {
        const list = conns.data?.filter((c) => c.platform === p) ?? [];
        const label = t(`platform.${p}`);
        return (
          <section key={p} aria-label={label} className="mb-6">
            <div className="mb-2 flex items-center justify-between">
              <h2 className="label-mono">{label}</h2>
              <Button size="sm" variant="ghost" aria-label={t("conn.connect", { platform: label })} onClick={() => setWizard(p)}>
                <Plus size={13} aria-hidden /> {t("conn.connect", { platform: label })}
              </Button>
            </div>
            <Card>
              {list.length ? (
                list.map((c) => <Row key={c.id} c={c} />)
              ) : (
                <p className="px-5 py-4 text-[13px] text-muted">{t("conn.empty", { platform: label })}</p>
              )}
            </Card>
          </section>
        );
      })}

      <Sheet
        open={wizard !== null}
        onOpenChange={(v) => !v && setWizard(null)}
        title={wizard === "meta_ads" ? t("conn.meta.title") : t("conn.google.title")}
        description={wizard === "meta_ads" ? t("conn.meta.desc") : t("conn.google.desc")}
        closeLabel={t("common.close")}
      >
        {wizard === "google_ads" && <GoogleWizard templatesUrl={templatesUrl} onDone={() => setWizard(null)} />}
        {wizard === "meta_ads" && <MetaWizard templatesUrl={templatesUrl} onDone={() => setWizard(null)} />}
      </Sheet>
    </>
  );
}
