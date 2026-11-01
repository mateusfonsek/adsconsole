import { useState } from "react";
import { useTranslation } from "react-i18next";
import * as Tabs from "@radix-ui/react-tabs";
import { commands, type AiClient, type Platform } from "../bindings";
import { useAction, useCmd } from "../lib/ipc";
import { PageHeader, SectionLabel } from "../components/layout/PageHeader";
import { Button } from "../components/ui/Button";
import { Card, CodeBlock, ErrorNote } from "../components/ui/Card";
import { inputClass } from "../components/ui/Field";
import { Segmented } from "../components/ui/Segmented";
import { StatusLabel } from "../components/ui/Status";
import { Switch } from "../components/ui/Switch";

const CLIENTS: AiClient[] = ["claude_code", "claude_desktop", "codex", "gemini_cli", "cursor"];
const PLATFORMS: Platform[] = ["google_ads", "meta_ads"];

function Snippet({ client, platform }: { client: AiClient; platform: Platform }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const preview = useCmd(["client_snippet_preview", client, platform], () => commands.clientSnippetPreview(client, platform));
  const copy = useAction(() => commands.copyClientSnippet(client, platform));
  return (
    <div className="flex flex-col gap-2">
      {preview.data && (
        <>
          <CodeBlock
            action={
              <Button
                size="sm"
                onClick={() =>
                  copy.mutate(undefined, {
                    onSuccess: () => {
                      setCopied(true);
                      setTimeout(() => setCopied(false), 2500);
                    },
                  })
                }
              >
                {t("mcps.copy_snippet")}
              </Button>
            }
          >
            {preview.data.text}
          </CodeBlock>
          <p className="text-xs text-muted">{copied ? t("mcps.copied_with_token") : t(preview.data.where_to_paste)}</p>
        </>
      )}
      {copy.error && <ErrorNote message={copy.error.message} code={copy.error.code} />}
    </div>
  );
}

export function Mcps() {
  const { t } = useTranslation();
  const status = useCmd(["server_status"], () => commands.serverStatus());
  const [token, setToken] = useState<string | null>(null);
  const [portDraft, setPortDraft] = useState<string | null>(null);
  const [platform, setPlatform] = useState<Platform>("google_ads");
  const toggle = useAction(
    ({ p, on }: { p: Platform; on: boolean }) => commands.setServerEnabled(p, on),
    ["server_status"],
  );
  const savePort = useAction((port: number) => commands.setPort(port), ["server_status", "get_app_settings"]);
  // `set_port` devolve Ok com o estado de erro quando a porta nova falha (a antiga volta).
  const [portFailure, setPortFailure] = useState<string | null>(null);

  const s = status.data;
  if (!s) return <PageHeader title={t("mcps.title")} subtitle={t("mcps.subtitle")} />;
  const port = s.state.port;
  const portValue = portDraft ?? String(port);

  const servers: { p: Platform; enabled: boolean; tools: number; mode: string }[] = [
    { p: "google_ads", enabled: s.google_enabled, tools: s.google_tools, mode: s.google_write_mode },
    { p: "meta_ads", enabled: s.meta_enabled, tools: s.meta_tools, mode: s.meta_write_mode },
  ];

  return (
    <>
      <PageHeader title={t("mcps.title")} subtitle={t("mcps.subtitle")} />

      {s.state.kind === "error" && (
        <Card className="mb-4 flex flex-col gap-3 border-err/40 p-4">
          <StatusLabel tone="err">{t("mcps.server_error")}</StatusLabel>
          <p className="text-[13px]">{s.state.error.message}</p>
        </Card>
      )}

      <div className="flex flex-col gap-3">
        {servers.map(({ p, enabled, tools, mode }) => (
          <Card key={p} className="p-4">
            <div className="flex items-center justify-between gap-4">
              <div>
                <div className="flex items-center gap-2">
                  <span className="text-[14px] font-semibold">{t(`platform.${p}`)}</span>
                  {s.state.kind === "running" && (
                    <StatusLabel tone={enabled ? "ok" : "muted"}>{enabled ? t("common.on") : t("home.paused")}</StatusLabel>
                  )}
                </div>
                <p className="mt-0.5 text-xs text-muted">
                  {enabled ? (
                    <>
                      {t("mcps.tools_count", { count: tools })} · {t("mcps.mode", { mode: t(`modes.${mode}`) })}
                    </>
                  ) : (
                    t("mcps.paused")
                  )}
                </p>
              </div>
              <Switch label={t(`platform.${p}`)} checked={enabled} onCheckedChange={(on) => toggle.mutate({ p, on })} />
            </div>
            <div className="mt-3 flex items-center gap-2">
              <span className="label-mono w-20">{t("mcps.address")}</span>
              <code className="font-mono text-xs">
                http://127.0.0.1:{port}
                {p === "google_ads" ? "/mcp/google-ads" : "/mcp/meta-ads"}
              </code>
            </div>
          </Card>
        ))}
      </div>

      <Card className="mt-3 flex flex-col gap-3 p-4">
        <div className="flex items-center gap-2">
          <label htmlFor="mcp-port" className="label-mono w-20">
            {t("mcps.port")}
          </label>
          <input
            id="mcp-port"
            inputMode="numeric"
            aria-invalid={savePort.error ? true : undefined}
            className={`${inputClass} w-24 font-mono text-xs`}
            value={portValue}
            onChange={(e) => setPortDraft(e.target.value.replace(/\D/g, ""))}
          />
          <Button
            size="sm"
            disabled={!portValue || (Number(portValue) === port && s.state.kind !== "error") || savePort.isPending}
            onClick={() =>
              savePort.mutate(Number(portValue), {
                onSuccess: (st) => {
                  if (st.kind === "error") {
                    setPortFailure(st.error.message);
                  } else {
                    setPortFailure(null);
                    setPortDraft(null);
                  }
                },
              })
            }
          >
            {t("mcps.save_port")}
          </Button>
        </div>
        {savePort.error && <ErrorNote message={savePort.error.message} code={savePort.error.code} />}
        {portFailure && <ErrorNote message={portFailure} />}
        <div className="flex items-center gap-2">
          <span className="label-mono w-20">{t("mcps.token")}</span>
          <code className="min-w-0 flex-1 truncate font-mono text-xs">{token ?? "••••••••••••••••"}</code>
          {token ? (
            <Button size="sm" variant="ghost" onClick={() => setToken(null)}>
              {t("mcps.hide_token")}
            </Button>
          ) : (
            <Button size="sm" variant="ghost" onClick={() => void commands.revealToken().then(setToken)}>
              {t("mcps.show_token")}
            </Button>
          )}
        </div>
      </Card>

      <SectionLabel>{t("mcps.connect_client")}</SectionLabel>
      <Card className="p-4">
        <div className="mb-3">
          <Segmented
            label={t("mcps.platform")}
            value={platform}
            options={PLATFORMS.map((p) => ({ value: p, label: t(`platform.${p}`) }))}
            onChange={setPlatform}
          />
        </div>
        <Tabs.Root defaultValue="claude_code">
          <Tabs.List aria-label={t("mcps.connect_client")} className="mb-3 flex gap-4 border-b border-line">
            {CLIENTS.map((c) => (
              <Tabs.Trigger
                key={c}
                value={c}
                className="-mb-px border-b-2 border-transparent pb-2 text-[13px] text-muted data-[state=active]:border-fg data-[state=active]:font-medium data-[state=active]:text-fg"
              >
                {t(`clients.${c}`)}
              </Tabs.Trigger>
            ))}
          </Tabs.List>
          {CLIENTS.map((c) => (
            <Tabs.Content key={c} value={c}>
              <Snippet client={c} platform={platform} />
            </Tabs.Content>
          ))}
        </Tabs.Root>
      </Card>
    </>
  );
}
