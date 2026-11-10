import { useEffect, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { commands, type PendingView } from "../bindings";
import { useAction, useCmd } from "../lib/ipc";
import { formatCountdown, formatDateTime, formatId, formatRelative } from "../lib/format";
import { navigate, useRoute } from "../lib/route";
import { useNow } from "../lib/useNow";
import { PageHeader } from "../components/layout/PageHeader";
import { ChangeDiff } from "../components/ChangeDiff";
import { Button } from "../components/ui/Button";
import { Card, ErrorNote } from "../components/ui/Card";

function Notice({ tone, children }: { tone: "warn" | "muted"; children: ReactNode }) {
  const cls = tone === "warn" ? "border-warn/40 bg-warn/10 text-fg" : "border-line bg-line/40 text-muted";
  return <div className={`rounded-console border px-3 py-2 text-xs ${cls}`}>{children}</div>;
}

function Detail({ p }: { p: PendingView }) {
  const { t, i18n } = useTranslation();
  const now = useNow();
  const inv = ["list_pending", "list_changes"];
  const approve = useAction(() => commands.approve(p.change.id), inv);
  const reject = useAction(() => commands.reject(p.change.id), inv);
  const c = p.change;
  const expired = new Date(p.expires_at).getTime() <= now;
  const aiWaiting = (new Date(p.ai_waiting_until).getTime() - now) / 1000;
  const busy = approve.isPending || reject.isPending;
  const error = approve.error ?? reject.error;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <span className="label-mono">{t(`platform.${c.platform}`)}</span>
          <h2 className="mt-1 text-[17px] font-semibold tracking-tight">{c.summary}</h2>
          <p className="mt-1 text-xs text-muted">
            {t("approvals.requested", { when: formatRelative(new Date(c.created_at), i18n.language) })}
          </p>
        </div>
        {!expired && aiWaiting > 0 && (
          <span className="label-mono shrink-0 rounded-[4px] bg-warn/15 px-2 py-1 text-warn">
            {t("approvals.ai_waiting", { time: formatCountdown(aiWaiting) })}
          </span>
        )}
      </div>

      <dl className="grid grid-cols-3 gap-3 text-xs">
        <div>
          <dt className="label-mono">{t("approvals.account")}</dt>
          <dd className="mt-0.5 font-mono" title={c.account_id}>
            {formatId(c.account_id)}
          </dd>
        </div>
        <div>
          <dt className="label-mono">{t("approvals.connection")}</dt>
          <dd className="mt-0.5">{c.connection_name ?? "—"}</dd>
        </div>
        <div>
          <dt className="label-mono">{t("approvals.tool")}</dt>
          <dd className="mt-0.5 font-mono">{c.tool}</dd>
        </div>
      </dl>

      <ChangeDiff
        before={c.before}
        after={c.after}
        labels={{ field: t("diff.field"), before: t("diff.before"), after: t("diff.after"), empty: t("diff.empty") }}
      />

      {!c.reversible && <Notice tone="warn">{t("approvals.irreversible")}</Notice>}
      {p.warning && <Notice tone="warn">{p.warning}</Notice>}
      {p.blocked_reason === "read_only" && <Notice tone="muted">{t("approvals.read_only")}</Notice>}
      {expired && <Notice tone="muted">{t("approvals.expired")}</Notice>}
      {error && <ErrorNote message={error.message} code={error.code} />}

      <div className="flex items-center gap-2">
        <Button
          variant="primary"
          disabled={expired || busy || !!p.blocked_reason}
          onClick={() => approve.mutate(undefined)}
        >
          {t("approvals.approve")}
        </Button>
        <Button disabled={expired || busy} onClick={() => reject.mutate(undefined)}>
          {t("approvals.reject")}
        </Button>
        {!expired && (
          <span className="ml-auto text-xs text-muted">
            {t("approvals.expires", { when: formatDateTime(new Date(p.expires_at), i18n.language) })}
          </span>
        )}
      </div>
    </div>
  );
}

/** Request that left the queue while its detail was open: stays on screen, read-only. */
function ClosedDetail({ id }: { id: string }) {
  const { t } = useTranslation();
  const c = useCmd(["get_change", id], () => commands.getChange(id)).data;
  if (!c) return null;
  return (
    <div className="flex flex-col gap-4">
      <div>
        <span className="label-mono">{t(`platform.${c.platform}`)}</span>
        <h2 className="mt-1 text-[17px] font-semibold tracking-tight">{c.summary}</h2>
      </div>
      <ChangeDiff
        before={c.before}
        after={c.after}
        labels={{ field: t("diff.field"), before: t("diff.before"), after: t("diff.after"), empty: t("diff.empty") }}
      />
      <Notice tone="muted">
        {c.status === "expired"
          ? t("approvals.expired")
          : t("approvals.closed", { status: t(`history.status_names.${c.status}`, c.status) })}
      </Notice>
    </div>
  );
}

export function Approvals() {
  const { t, i18n } = useTranslation();
  const route = useRoute();
  const pending = useCmd(["list_pending"], () => commands.listPending());
  const list = pending.data ?? [];
  // The open request lives in the URL: if it leaves the queue, the screen does not switch to another one by itself.
  const selectedId = route.param ?? list[0]?.change.id;
  const selected = list.find((p) => p.change.id === selectedId);
  const firstId = list[0]?.change.id;
  useEffect(() => {
    // Only while this screen is active: when navigating to another route, it does not pull the user back.
    if (route.name === "approvals" && !route.param && firstId) navigate("approvals", firstId);
  }, [route.name, route.param, firstId]);

  return (
    <>
      <PageHeader title={t("approvals.title")} subtitle={t("approvals.subtitle")} />
      {pending.data && !list.length && !selectedId ? (
        <Card className="px-5 py-10 text-center">
          <p className="text-[13px] font-medium">{t("approvals.empty")}</p>
          <p className="mt-1 text-xs text-muted">{t("approvals.empty_hint")}</p>
        </Card>
      ) : (
        <div className="flex gap-4">
          <ul className="flex w-64 shrink-0 flex-col gap-1">
            {list.map((p) => {
              const active = p.change.id === selected?.change.id;
              return (
                <li key={p.change.id}>
                  <button
                    type="button"
                    aria-current={active ? "true" : undefined}
                    onClick={() => navigate("approvals", p.change.id)}
                    className={`w-full rounded-console px-3 py-2.5 text-left transition-colors ${
                      active ? "bg-line" : "hover:bg-line/50"
                    }`}
                  >
                    <span className="block truncate text-[13px] font-medium">{p.change.summary}</span>
                    <span className="mt-0.5 block truncate text-xs text-muted">
                      {t(`platform.${p.change.platform}`)} · {formatRelative(new Date(p.change.created_at), i18n.language)}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
          {selectedId && (
            <Card className="min-w-0 flex-1 p-5">
              {selected ? <Detail key={selected.change.id} p={selected} /> : <ClosedDetail key={selectedId} id={selectedId} />}
            </Card>
          )}
        </div>
      )}
    </>
  );
}
