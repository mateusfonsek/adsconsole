import { useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type ChangeView, type Platform } from "../bindings";
import { useAction, useCmd } from "../lib/ipc";
import { formatDateTime } from "../lib/format";
import { PageHeader } from "../components/layout/PageHeader";
import { ChangeDiff } from "../components/ChangeDiff";
import { Button } from "../components/ui/Button";
import { Card, ErrorNote } from "../components/ui/Card";
import { Segmented } from "../components/ui/Segmented";
import { Sheet } from "../components/ui/Sheet";
import { StatusLabel, type Tone } from "../components/ui/Status";

const PAGE = 50;
type StatusFilter = "all" | "applied" | "failed" | "rejected" | "pending";
type Period = "today" | "week" | "month" | "ever";
type PlatformFilter = "all" | Platform;

const TONE: Record<string, Tone> = { applied: "ok", failed: "err", rejected: "muted", expired: "muted", pending: "warn", approved: "warn" };

function since(p: Period): string | null {
  const day = 24 * 3600 * 1000;
  const d = { today: day, week: 7 * day, month: 30 * day, ever: 0 }[p];
  return d ? new Date(Date.now() - d).toISOString() : null;
}

function Row({ c, onOpen }: { c: ChangeView; onOpen: () => void }) {
  const { t, i18n } = useTranslation();
  const [done, setDone] = useState<string | null>(null);
  const undo = useAction(() => commands.undo(c.id), ["list_changes", "list_pending", "get_change"]);
  return (
    <>
      <tr className="border-b border-line/70 last:border-b-0">
        <td className="whitespace-nowrap py-2.5 pl-5 pr-4 font-mono text-[11.5px] text-muted">
          {formatDateTime(new Date(c.created_at), i18n.language)}
        </td>
        <td className="py-2.5 pr-4 text-xs">{t(`platform.${c.platform}`)}</td>
        <td className="py-2.5 pr-4">
          <button type="button" onClick={onOpen} className="text-left text-[13px] hover:underline">
            {c.summary}
          </button>
        </td>
        <td className="py-2.5 pr-4">
          <StatusLabel tone={TONE[c.status] ?? "muted"}>{t(`history.status_names.${c.status}`, c.status)}</StatusLabel>
        </td>
        <td className="py-2.5 pr-5 text-right">
          {c.undone_by ? (
            <span className="label-mono">{t("history.undone")}</span>
          ) : c.status === "applied" && !c.reversible ? (
            <span className="label-mono">{t("history.irreversible")}</span>
          ) : c.status === "applied" ? (
            <Button
              size="sm"
              disabled={undo.isPending}
              onClick={() =>
                undo.mutate(undefined, {
                  onSuccess: (r) => setDone(r.status === "pending" ? t("history.undo_pending") : t("history.undo_ok")),
                })
              }
            >
              {t("history.undo")}
            </Button>
          ) : null}
        </td>
      </tr>
      {(undo.error || done) && (
        <tr>
          <td colSpan={5} className="px-5 pb-3">
            {undo.error ? <ErrorNote message={undo.error.message} code={undo.error.code} /> : <span className="text-xs text-muted">{done}</span>}
          </td>
        </tr>
      )}
    </>
  );
}

function Detail({ id }: { id: string }) {
  const { t } = useTranslation();
  const c = useCmd(["get_change", id], () => commands.getChange(id)).data;
  if (!c) return null;
  const err = c.error as { message?: string; code?: string } | null;
  return (
    <div className="flex flex-col gap-4">
      <ChangeDiff
        before={c.before}
        after={c.after}
        labels={{ field: t("diff.field"), before: t("diff.before"), after: t("diff.after"), empty: t("diff.empty") }}
      />
      {err?.message && <ErrorNote message={err.message} code={err.code} />}
      {c.orphaned_ids.length > 0 && (
        <div className="flex flex-col gap-1.5">
          <p className="text-xs">{t("history.orphans")}</p>
          <ul className="flex flex-col gap-1">
            {c.orphaned_ids.map((o) => (
              <li key={o}>
                <code className="select-text font-mono text-xs">{o}</code>
              </li>
            ))}
          </ul>
        </div>
      )}
      <p className="font-mono text-[11px] text-muted">
        {c.tool} · {c.id}
      </p>
    </div>
  );
}

export function History() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<StatusFilter>("all");
  const [period, setPeriod] = useState<Period>("week");
  const [platform, setPlatform] = useState<PlatformFilter>("all");
  const [limit, setLimit] = useState(PAGE);
  const [open, setOpen] = useState<ChangeView | null>(null);

  const filter = {
    platform: platform === "all" ? null : platform,
    account_id: null,
    status: status === "all" ? null : status,
    since: since(period),
    limit,
    offset: 0,
  };
  const changes = useCmd(["list_changes", status, period, platform, limit], () => commands.listChanges(filter));
  const list = changes.data ?? [];

  return (
    <>
      <PageHeader title={t("history.title")} subtitle={t("history.subtitle")} />
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <Segmented<StatusFilter>
          label={t("history.filter_status")}
          value={status}
          options={(["all", "applied", "failed", "rejected", "pending"] as StatusFilter[]).map((s) => ({ value: s, label: t(`history.${s}`) }))}
          onChange={setStatus}
        />
        <Segmented<Period>
          label={t("history.filter_period")}
          value={period}
          options={(["today", "week", "month", "ever"] as Period[]).map((p) => ({ value: p, label: t(`history.${p}`) }))}
          onChange={setPeriod}
        />
        <Segmented<PlatformFilter>
          label={t("history.filter_platform")}
          value={platform}
          options={[
            { value: "all", label: t("history.all") },
            { value: "google_ads", label: t("platform.google_ads") },
            { value: "meta_ads", label: t("platform.meta_ads") },
          ]}
          onChange={setPlatform}
        />
      </div>
      {changes.error && <ErrorNote message={changes.error.message} code={changes.error.code} />}
      <Card>
        {changes.data && !list.length ? (
          <p className="px-5 py-8 text-center text-[13px] text-muted">{t("history.empty")}</p>
        ) : (
          <table className="w-full border-collapse">
            <thead>
              <tr className="border-b border-line">
                {[t("history.when"), t("history.platform"), t("history.what"), t("history.status"), ""].map((h, i) => (
                  <th key={i} className={`label-mono py-2.5 pr-4 text-left font-medium ${i === 0 ? "pl-5" : ""}`}>
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {list.map((c) => (
                <Row key={c.id} c={c} onOpen={() => setOpen(c)} />
              ))}
            </tbody>
          </table>
        )}
      </Card>
      {list.length >= limit && (
        <div className="mt-3 text-center">
          <Button size="sm" variant="ghost" onClick={() => setLimit(limit + PAGE)}>
            {t("history.load_more")}
          </Button>
        </div>
      )}
      <Sheet
        open={open !== null}
        onOpenChange={(v) => !v && setOpen(null)}
        title={open?.summary ?? ""}
        description={open ? t(`platform.${open.platform}`) : undefined}
        closeLabel={t("common.close")}
      >
        {open && <Detail id={open.id} />}
      </Sheet>
    </>
  );
}
