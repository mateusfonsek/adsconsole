import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight } from "lucide-react";
import { commands, type GroupView, type Platform, type ToolGroup, type WriteMode } from "../bindings";
import { useAction, useCmd } from "../lib/ipc";
import { PageHeader } from "../components/layout/PageHeader";
import { Card } from "../components/ui/Card";
import { Field } from "../components/ui/Field";
import { Segmented } from "../components/ui/Segmented";
import { Switch } from "../components/ui/Switch";

const MODES: WriteMode[] = ["read_only", "approval", "direct"];

function Group({ platform, g }: { platform: Platform; g: GroupView }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const inv = ["get_tool_settings", "server_status"];
  const setGroup = useAction((on: boolean) => commands.setToolGroup(platform, g.group as ToolGroup, on), inv);
  const setTool = useAction(({ name, on }: { name: string; on: boolean }) => commands.setTool(platform, name, on), inv);
  const label = t(`tools.groups.${g.group}`);
  const enabledCount = g.tools.filter((x) => x.enabled).length;

  return (
    <div className="border-b border-line last:border-b-0">
      <div className="flex items-center gap-3 px-5 py-3.5">
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen(!open)}
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
        >
          <ChevronRight size={14} aria-hidden className={`shrink-0 text-muted transition-transform ${open ? "rotate-90" : ""}`} />
          <span className="min-w-0">
            <span className="block text-[13px] font-medium">{label}</span>
            <span className="block text-xs text-muted">{t(`tools.group_hint.${g.group}`)}</span>
          </span>
        </button>
        <span className="label-mono">
          {enabledCount}/{g.tools.length}
        </span>
        <Switch label={label} checked={g.enabled} onCheckedChange={(on) => setGroup.mutate(on)} />
      </div>
      {open && (
        <ul className="border-t border-line bg-bg/60">
          {g.tools.map((tool) => (
            <li key={tool.name} className="flex items-center gap-3 py-2 pl-11 pr-5">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <code className="font-mono text-xs">{tool.name}</code>
                  {tool.destructive && <span className="label-mono text-warn">{t("tools.destructive")}</span>}
                </div>
                <p className="truncate text-xs text-muted" title={tool.description}>
                  {tool.description}
                </p>
              </div>
              <Switch label={tool.name} checked={tool.enabled} onCheckedChange={(on) => setTool.mutate({ name: tool.name, on })} />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Wait({ platform, secs }: { platform: Platform; secs: number }) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState<string | null>(null);
  const save = useAction((v: number) => commands.setApprovalWait(platform, v), ["get_tool_settings"]);
  const value = draft ?? String(secs);
  return (
    <div className="w-72">
      <Field
        label={t("tools.wait")}
        hint={t("tools.wait_hint")}
        error={save.error?.message}
        mono
        inputMode="numeric"
        value={value}
        onChange={(e) => setDraft(e.target.value.replace(/\D/g, ""))}
        onBlur={() => {
          if (value && Number(value) !== secs) save.mutate(Number(value), { onSuccess: () => setDraft(null) });
        }}
      />
    </div>
  );
}

export function Tools() {
  const { t } = useTranslation();
  const [platform, setPlatform] = useState<Platform>("google_ads");
  const view = useCmd(["get_tool_settings", platform], () => commands.getToolSettings(platform));
  const setMode = useAction((mode: WriteMode) => commands.setWriteMode(platform, mode), ["get_tool_settings", "server_status", "list_pending"]);
  const v = view.data;

  return (
    <>
      <PageHeader
        title={t("tools.title")}
        subtitle={t("tools.subtitle")}
        actions={
          <Segmented
            label={t("mcps.platform")}
            value={platform}
            options={(["google_ads", "meta_ads"] as Platform[]).map((p) => ({ value: p, label: t(`platform.${p}`) }))}
            onChange={setPlatform}
          />
        }
      />
      {v && (
        <>
          <Card className="mb-4 flex flex-col gap-4 p-5">
            <div className="flex flex-col gap-1.5">
              <span className="text-[13px] font-medium">{t("tools.write_mode")}</span>
              <Segmented
                label={t("tools.write_mode")}
                value={v.write_mode}
                options={MODES.map((m) => ({ value: m, label: t(`modes.${m}`) }))}
                onChange={(m) => setMode.mutate(m)}
              />
              <span className="text-xs text-muted">{t("tools.write_mode_hint")}</span>
            </div>
            <Wait key={platform} platform={platform} secs={v.approval_wait_secs} />
          </Card>
          <Card>
            {v.groups.map((g) => (
              <Group key={g.group} platform={platform} g={g} />
            ))}
          </Card>
        </>
      )}
    </>
  );
}
