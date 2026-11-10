import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { FileJson } from "lucide-react";
import { commands, type AccessLevelView, type ConsoleLinksView } from "../../bindings";
import { asAppError, useAction, type AppErr } from "../../lib/ipc";
import { Button } from "../../components/ui/Button";
import { ErrorNote } from "../../components/ui/Card";
import { ExternalLink } from "../../components/ui/ExternalLink";
import { Field } from "../../components/ui/Field";
import { Segmented } from "../../components/ui/Segmented";
import { WaitingLogin, WizardStep } from "./shared";

const LEVELS: AccessLevelView[] = ["test", "explorer", "basic", "standard"];

export function GoogleWizard({ templatesUrl, onDone }: { templatesUrl: string; onDone: () => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState("");
  const [path, setPath] = useState<string | null>(null);
  const [links, setLinks] = useState<ConsoleLinksView | null>(null);
  const [fileError, setFileError] = useState<AppErr | null>(null);
  const [loginCustomer, setLoginCustomer] = useState("");
  const [level, setLevel] = useState<AccessLevelView>("explorer");

  const add = useAction(
    () =>
      commands.addGoogleConnection({
        name,
        client_json_path: path ?? "",
        login_customer_id: loginCustomer || null,
        access_level: level,
      }),
    ["list_connections", "onboarding_state"],
  );

  async function pick(p: string | null) {
    if (!p) return;
    setPath(p);
    setLinks(null);
    setFileError(null);
    try {
      setLinks(await commands.googleConsoleLinks(p));
    } catch (e) {
      setFileError(asAppError(e));
    }
  }

  // Dragging the file onto the window delivers its path (Rust reads the contents).
  useEffect(() => {
    let stop: (() => void) | undefined;
    try {
      void getCurrentWebview()
        .onDragDropEvent((e) => {
          if (e.payload.type === "drop") {
            const json = e.payload.paths.find((p) => p.toLowerCase().endsWith(".json"));
            if (json) void pick(json);
          }
        })
        .then((un) => (stop = un))
        .catch(() => {});
    } catch {
      // Outside Tauri (tests): no dragging.
    }
    return () => stop?.();
  }, []);

  if (add.isPending) return <WaitingLogin onCancel={() => void commands.cancelLogin()} />;

  const ready = !!name.trim() && !!path && !fileError;
  const submitError = add.error && add.error.code !== "LOGIN_CANCELLED" ? add.error : null;

  return (
    <div className="flex flex-col gap-4">
      <ol>
        <WizardStep n={1} title={t("conn.google.step1")}>
          <ExternalLink href={links?.api ?? "https://console.cloud.google.com/"}>
            {links ? t("conn.google.open_api") : t("conn.google.open_console")}
          </ExternalLink>
        </WizardStep>
        <WizardStep n={2} title={t("conn.google.step2")}>
          {links && <ExternalLink href={links.consent}>{t("conn.google.open_consent")}</ExternalLink>}
          <ExternalLink href={templatesUrl}>{t("conn.templates")}</ExternalLink>
        </WizardStep>
        <WizardStep n={3} title={t("conn.google.step3")}>
          {links && <ExternalLink href={links.clients}>{t("conn.google.open_clients")}</ExternalLink>}
        </WizardStep>
      </ol>

      <div
        className={`flex flex-col items-center gap-2 rounded-console border border-dashed px-4 py-5 text-center ${
          fileError ? "border-err/60" : "border-line-strong"
        }`}
      >
        <FileJson size={18} strokeWidth={1.5} aria-hidden className="text-muted" />
        {path ? (
          <span className="max-w-full truncate font-mono text-[11px]" title={path}>
            {path.split(/[\\/]/).pop()}
          </span>
        ) : (
          <span className="text-xs text-muted">{t("conn.google.drop")}</span>
        )}
        <Button
          size="sm"
          onClick={async () => {
            const chosen = await openDialog({ multiple: false, directory: false, filters: [{ name: "JSON", extensions: ["json"] }] });
            await pick(typeof chosen === "string" ? chosen : null);
          }}
        >
          {t("conn.google.choose")}
        </Button>
        {fileError && <ErrorNote message={fileError.message} />}
      </div>

      <Field label={t("conn.name")} hint={t("conn.name_hint")} value={name} onChange={(e) => setName(e.target.value)} />
      <Field
        label={t("conn.google.login_customer")}
        hint={t("conn.google.login_customer_hint")}
        mono
        value={loginCustomer}
        onChange={(e) => setLoginCustomer(e.target.value)}
      />
      <div className="flex flex-col gap-1">
        <span className="text-xs font-medium">{t("conn.google.access")}</span>
        <Segmented
          label={t("conn.google.access")}
          value={level}
          options={LEVELS.map((l) => ({ value: l, label: l[0].toUpperCase() + l.slice(1) }))}
          onChange={setLevel}
        />
      </div>

      {submitError && <ErrorNote message={submitError.message} code={submitError.code} />}
      <div>
        <Button variant="primary" disabled={!ready} onClick={() => add.mutate(undefined, { onSuccess: onDone })}>
          {t("conn.google.submit")}
        </Button>
      </div>
    </div>
  );
}
