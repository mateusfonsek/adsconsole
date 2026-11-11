import { useTranslation } from "react-i18next";
import {
  CheckCheck,
  History,
  House,
  type LucideIcon,
  PlugZap,
  Server,
  Settings,
  SlidersHorizontal,
} from "lucide-react";
import { commands } from "../../bindings";
import { useCmd } from "../../lib/ipc";
import { href, type RouteName } from "../../lib/route";
import { BrandMark } from "../BrandMark";
import { CountBadge } from "../ui/Status";

const ITEMS: [RouteName, LucideIcon][] = [
  ["home", House],
  ["connections", PlugZap],
  ["mcps", Server],
  ["tools", SlidersHorizontal],
  ["approvals", CheckCheck],
  ["history", History],
];

function Item({ name, icon: Icon, active, badge }: { name: RouteName; icon: LucideIcon; active: boolean; badge?: number }) {
  const { t } = useTranslation();
  return (
    <a
      href={href(name)}
      aria-current={active ? "page" : undefined}
      className={`flex h-8 items-center gap-2.5 rounded-console px-2.5 text-[13px] transition-colors ${
        active ? "bg-line font-medium text-fg" : "text-muted hover:bg-line/60 hover:text-fg"
      }`}
    >
      <Icon size={15} strokeWidth={1.75} aria-hidden />
      <span className="flex-1">{t(`nav.${name}`)}</span>
      {badge ? <CountBadge count={badge} label={t("nav.pending_count", { count: badge })} /> : null}
    </a>
  );
}

export function Sidebar({ route, version }: { route: RouteName; version?: string }) {
  const { t } = useTranslation();
  const pending = useCmd(["list_pending"], () => commands.listPending());
  const count = pending.data?.length ?? 0;
  return (
    <nav aria-label="AdsConsole" className="flex w-[196px] shrink-0 flex-col border-r border-line bg-bg px-3 pb-3 pt-4">
      <div className="mb-5 flex items-center gap-2 px-2.5" data-tauri-drag-region>
        <BrandMark />
        <span className="text-[13px] font-semibold tracking-tight">{t("app.name")}</span>
      </div>
      <div className="flex flex-col gap-0.5">
        {ITEMS.map(([name, icon]) => (
          <Item key={name} name={name} icon={icon} active={route === name} badge={name === "approvals" ? count : undefined} />
        ))}
      </div>
      <div className="flex-1" />
      <Item name="settings" icon={Settings} active={route === "settings"} />
      {version && <span className="label-mono mt-3 px-2.5">v{version}</span>}
    </nav>
  );
}
