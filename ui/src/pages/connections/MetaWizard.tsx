import { useState } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "../../bindings";
import { useAction } from "../../lib/ipc";
import { Button } from "../../components/ui/Button";
import { ErrorNote } from "../../components/ui/Card";
import { ExternalLink } from "../../components/ui/ExternalLink";
import { Field } from "../../components/ui/Field";
import { WaitingLogin, WizardStep } from "./shared";

const APPS = "https://developers.facebook.com/apps/";

export function MetaWizard({ templatesUrl, onDone }: { templatesUrl: string; onDone: () => void }) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<"login" | "token">("login");
  const [name, setName] = useState("");
  const [appId, setAppId] = useState("");
  const [configId, setConfigId] = useState("");
  const [secret, setSecret] = useState("");
  const [relay, setRelay] = useState("");
  const [token, setToken] = useState("");

  const invalidate = ["list_connections", "onboarding_state"];
  const login = useAction(
    () =>
      commands.addMetaLogin({
        name,
        app_id: appId.trim(),
        config_id: configId.trim(),
        app_secret: secret,
        relay_url: relay.trim() || null,
      }),
    invalidate,
  );
  const byToken = useAction(
    () =>
      commands.addMetaToken({
        name,
        access_token: token.trim(),
        app_id: appId.trim() || null,
        app_secret: secret || null,
      }),
    invalidate,
  );

  if (login.isPending) return <WaitingLogin onCancel={() => void commands.cancelLogin()} />;

  const id = appId.trim();
  // O App Secret nunca fica no formulário depois de uma tentativa.
  const clearSecret = () => setSecret("");
  const error = [login.error, byToken.error].find((e) => e && e.code !== "LOGIN_CANCELLED");
  const loginReady = !!name.trim() && !!id && !!configId.trim() && !!secret;
  const tokenReady = !!name.trim() && !!token.trim();

  return (
    <div className="flex flex-col gap-4">
      <ol>
        <WizardStep n={1} title={t("conn.meta.step1")}>
          <ExternalLink href={APPS}>{t("conn.meta.open_apps")}</ExternalLink>
        </WizardStep>
        <WizardStep n={2} title={t("conn.meta.step2")}>
          {id && (
            <>
              <ExternalLink href={`${APPS}${encodeURIComponent(id)}/settings/basic/`}>{t("conn.meta.open_basic")}</ExternalLink>
              <ExternalLink href={`${APPS}${encodeURIComponent(id)}/business-login/settings/`}>{t("conn.meta.open_login")}</ExternalLink>
            </>
          )}
        </WizardStep>
        <WizardStep n={3} title={t("conn.meta.step3")}>
          <ExternalLink href={templatesUrl}>{t("conn.templates")}</ExternalLink>
        </WizardStep>
      </ol>

      <Field label={t("conn.name")} hint={t("conn.name_hint")} value={name} onChange={(e) => setName(e.target.value)} />
      <div className="grid grid-cols-2 gap-3">
        <Field label={t("conn.meta.app_id")} mono value={appId} onChange={(e) => setAppId(e.target.value)} />
        {mode === "login" && (
          <Field label={t("conn.meta.config_id")} mono value={configId} onChange={(e) => setConfigId(e.target.value)} />
        )}
      </div>
      <Field
        label={t("conn.meta.app_secret")}
        type="password"
        autoComplete="off"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
      />
      {mode === "login" ? (
        <Field
          label={t("conn.meta.relay")}
          hint={t("conn.meta.relay_hint")}
          mono
          placeholder="https://"
          value={relay}
          onChange={(e) => setRelay(e.target.value)}
        />
      ) : (
        <Field
          label={t("conn.meta.token")}
          hint={t("conn.meta.token_hint")}
          type="password"
          autoComplete="off"
          value={token}
          onChange={(e) => setToken(e.target.value)}
        />
      )}

      {error && <ErrorNote message={error.message} code={error.code} />}
      <div className="flex items-center gap-2">
        {mode === "login" ? (
          <Button
            variant="primary"
            disabled={!loginReady}
            onClick={() => login.mutate(undefined, { onSuccess: onDone, onSettled: clearSecret })}
          >
            {t("conn.meta.submit")}
          </Button>
        ) : (
          <Button
            variant="primary"
            disabled={!tokenReady || byToken.isPending}
            onClick={() => byToken.mutate(undefined, { onSuccess: onDone, onSettled: () => (clearSecret(), setToken("")) })}
          >
            {t("conn.meta.save_token")}
          </Button>
        )}
        <Button variant="ghost" onClick={() => setMode(mode === "login" ? "token" : "login")}>
          {mode === "login" ? t("conn.meta.use_token") : t("conn.meta.use_login")}
        </Button>
      </div>
    </div>
  );
}
