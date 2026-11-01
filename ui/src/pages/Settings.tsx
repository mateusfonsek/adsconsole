import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import { commands, type Theme } from "../bindings";
import { applyTheme } from "../lib/theme";
import { useAction, useCmd } from "../lib/ipc";
import { PageHeader } from "../components/layout/PageHeader";
import { Button } from "../components/ui/Button";
import { Card } from "../components/ui/Card";
import { inputClass } from "../components/ui/Field";
import { Segmented } from "../components/ui/Segmented";
import { Switch } from "../components/ui/Switch";

function Row({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-6 border-b border-line px-5 py-4 last:border-b-0">
      <div className="min-w-0">
        <div className="text-[13px] font-medium">{label}</div>
        {hint && <div className="mt-0.5 text-xs text-muted">{hint}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}

export function Settings() {
  const { t, i18n } = useTranslation();
  const app = useCmd(["get_app_settings"], () => commands.getAppSettings());
  // `null` enquanto o usuário não editou: o campo mostra a porta salva.
  const [portDraft, setPortDraft] = useState<string | null>(null);
  const [updateMsg, setUpdateMsg] = useState<string | null>(null);

  const setLanguage = useAction((lang: string) => commands.setLanguage(lang), ["get_app_settings"]);
  const setTheme = useAction((theme: Theme) => commands.setTheme(theme), ["get_app_settings"]);
  const setAutostart = useAction((on: boolean) => commands.setAutostart(on), ["get_app_settings"]);
  const savePort = useAction((p: number) => commands.setPort(p), ["get_app_settings", "server_status"]);
  const server = useCmd(["server_status"], () => commands.serverStatus());
  const [portFailure, setPortFailure] = useState<string | null>(null);
  const check = useAction(() => commands.checkUpdate());

  const s = app.data;
  if (!s) return <PageHeader title={t("settings.title")} />;

  const portError = savePort.error?.message ?? portFailure;
  const port = portDraft ?? String(s.port);

  return (
    <>
      <PageHeader title={t("settings.title")} />
      <Card>
        <Row label={t("settings.language")}>
          <Segmented
            label={t("settings.language")}
            value={s.language}
            options={[
              { value: "pt", label: "Português" },
              { value: "en", label: "English" },
            ]}
            onChange={(lang) =>
              setLanguage.mutate(lang, { onSuccess: () => void i18n.changeLanguage(lang) })
            }
          />
        </Row>
        <Row label={t("settings.theme")}>
          <Segmented<Theme>
            label={t("settings.theme")}
            value={s.theme}
            options={[
              { value: "system", label: t("settings.theme_system") },
              { value: "light", label: t("settings.theme_light") },
              { value: "dark", label: t("settings.theme_dark") },
            ]}
            onChange={(theme) => setTheme.mutate(theme, { onSuccess: () => applyTheme(theme) })}
          />
        </Row>
        <Row label={t("settings.autostart")} hint={t("settings.autostart_hint")}>
          <Switch
            label={t("settings.autostart")}
            checked={s.autostart}
            onCheckedChange={(on) => setAutostart.mutate(on)}
          />
        </Row>
        <Row
          label={<label htmlFor="settings-port">{t("settings.port")}</label>}
          hint={portError ? <span className="text-err">{portError}</span> : t("settings.port_hint")}
        >
          <input
            id="settings-port"
            inputMode="numeric"
            aria-invalid={portError ? true : undefined}
            className={`${inputClass} w-24 font-mono text-xs`}
            value={port}
            onChange={(e) => setPortDraft(e.target.value.replace(/\D/g, ""))}
          />
          <Button
            disabled={!port || (Number(port) === s.port && server.data?.state.kind !== "error") || savePort.isPending}
            onClick={() =>
              savePort.mutate(Number(port), {
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
            {t("settings.save_port")}
          </Button>
        </Row>
        <Row
          label={t("settings.secrets")}
          hint={s.secret_backend === "file" ? t("settings.secrets_file") : t("settings.secrets_keychain")}
        >
          <span className="label-mono">{s.secret_backend ?? "—"}</span>
        </Row>
        <Row label={t("settings.data_dir")} hint={<span className="font-mono text-[11px]">{s.data_dir}</span>}>
          <Button size="sm" onClick={() => void commands.openDataDir()}>
            {t("settings.open_folder")}
          </Button>
        </Row>
        <Row label={t("settings.updates")} hint={updateMsg ?? t("settings.version", { version: s.version })}>
          <Button
            size="sm"
            disabled={check.isPending}
            onClick={() =>
              check.mutate(undefined, {
                onSuccess: (u) =>
                  setUpdateMsg(u ? t("common.update_available", { version: u.version }) : t("settings.up_to_date")),
              })
            }
          >
            {t("settings.check_updates")}
          </Button>
        </Row>
        <Row label={t("settings.project")} hint={<span className="font-mono text-[11px]">{s.repo_url}</span>}>
          <Button size="sm" variant="ghost" onClick={() => void openUrl(s.repo_url)}>
            {t("common.open")}
          </Button>
        </Row>
      </Card>
    </>
  );
}
