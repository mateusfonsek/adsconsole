import { useTranslation } from "react-i18next";
import { ArrowRight, Check } from "lucide-react";
import { commands } from "../bindings";
import { useCmd } from "../lib/ipc";
import { href, type RouteName } from "../lib/route";
import { PageHeader, SectionLabel } from "../components/layout/PageHeader";
import { Card } from "../components/ui/Card";
import { StatusLabel, type Tone } from "../components/ui/Status";

function Step({ n, done, title, hint, to }: { n: number; done: boolean; title: string; hint: string; to?: RouteName }) {
  const { t } = useTranslation();
  return (
    <li data-done={done} className="flex items-start gap-4 border-b border-line px-5 py-4 last:border-b-0">
      <span
        aria-hidden
        className={`mt-0.5 grid h-5 w-5 shrink-0 place-items-center rounded-[4px] font-mono text-[11px] ${
          done ? "bg-fg text-bg" : "border border-line-strong text-muted"
        }`}
      >
        {done ? <Check size={12} strokeWidth={2.5} /> : n}
      </span>
      <div className="flex-1">
        <p className={`text-[13px] font-medium ${done ? "text-muted line-through decoration-line-strong" : ""}`}>{title}</p>
        <p className="mt-0.5 text-xs text-muted">{hint}</p>
      </div>
      {done ? (
        <span className="label-mono">{t("home.done")}</span>
      ) : (
        to && (
          <a href={href(to)} className="inline-flex items-center gap-1 text-xs font-medium hover:underline">
            {t("home.go")} <ArrowRight size={13} aria-hidden />
          </a>
        )
      )}
    </li>
  );
}

function StatusCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Card className="flex flex-col gap-2 p-4">
      <span className="label-mono">{title}</span>
      {children}
    </Card>
  );
}

export function Home() {
  const { t } = useTranslation();
  const onboarding = useCmd(["onboarding_state"], () => commands.onboardingState());
  const server = useCmd(["server_status"], () => commands.serverStatus());
  const conns = useCmd(["list_connections"], () => commands.listConnections());
  const pending = useCmd(["list_pending"], () => commands.listPending());

  const o = onboarding.data;
  const clientStepDone = !!o?.client_seen;
  const showSteps = o && !(o.has_connection && o.client_seen);

  const reconnect = conns.data?.filter((c) => c.status.kind === "needs_reconnect").length ?? 0;
  const expiring = conns.data?.filter((c) => c.status.kind === "expiring_soon").length ?? 0;
  const pendingCount = pending.data?.length ?? 0;

  const s = server.data;
  const serverTone: Tone = s?.state.kind === "error" ? "err" : "ok";

  return (
    <>
      <PageHeader title={t("home.title")} subtitle={t("home.subtitle")} />

      {showSteps && (
        <section className="mb-8">
          <SectionLabel>{t("home.steps_title")}</SectionLabel>
          <Card>
            <ol aria-label={t("home.steps_title")}>
              <Step n={1} done={o.has_connection} title={t("home.step_connect")} hint={t("home.step_connect_hint")} to="connections" />
              <Step n={2} done={clientStepDone} title={t("home.step_client")} hint={t("home.step_client_hint")} to="mcps" />
              <Step n={3} done={o.client_seen} title={t("home.step_first_call")} hint={t("home.step_first_call_hint")} />
            </ol>
          </Card>
        </section>
      )}

      <div className="grid grid-cols-3 gap-3">
        <StatusCard title={t("home.mcps")}>
          {s && (
            <>
              <StatusLabel tone={serverTone}>
                {s.state.kind === "error" ? t("home.server_error") : t("home.server_running")}
              </StatusLabel>
              <span className="font-mono text-xs">127.0.0.1:{s.state.port}</span>
              <span className="text-xs text-muted">
                {t("platform.google_ads")} · {s.google_enabled ? t("common.on") : t("home.paused")}
                <br />
                {t("platform.meta_ads")} · {s.meta_enabled ? t("common.on") : t("home.paused")}
              </span>
            </>
          )}
        </StatusCard>
        <StatusCard title={t("home.connections")}>
          {conns.data && (
            <>
              <span className="text-[13px]">{t("home.connections_count", { count: conns.data.length })}</span>
              {reconnect > 0 && <StatusLabel tone="err">{t("home.reconnect_count", { count: reconnect })}</StatusLabel>}
              {expiring > 0 && <StatusLabel tone="warn">{t("home.expiring_count", { count: expiring })}</StatusLabel>}
              {reconnect === 0 && expiring === 0 && <StatusLabel tone="ok">{t("home.all_good")}</StatusLabel>}
            </>
          )}
        </StatusCard>
        <StatusCard title={t("home.approvals")}>
          {pendingCount > 0 ? (
            <a href={href("approvals")} className="inline-flex items-center gap-1 text-[13px] font-medium hover:underline">
              {t("nav.pending_count", { count: pendingCount })} <ArrowRight size={13} aria-hidden />
            </a>
          ) : (
            <span className="text-[13px] text-muted">{t("home.no_pending")}</span>
          )}
        </StatusCard>
      </div>
    </>
  );
}
