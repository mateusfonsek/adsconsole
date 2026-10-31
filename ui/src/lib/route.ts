import { useEffect, useState } from "react";

export type RouteName = "home" | "connections" | "mcps" | "tools" | "approvals" | "history" | "settings";

export type Route = { name: RouteName; param?: string };

const NAMES: RouteName[] = ["home", "connections", "mcps", "tools", "approvals", "history", "settings"];

/** `#/approvals/chg_1` → `{ name: "approvals", param: "chg_1" }`. */
export function parseHash(hash: string): Route {
  const [first, param] = hash.replace(/^#\/?/, "").split("/");
  const name = (NAMES as string[]).includes(first) ? (first as RouteName) : "home";
  return param ? { name, param: decodeURIComponent(param) } : { name };
}

export function href(name: RouteName, param?: string): string {
  if (name === "home") return "#/";
  return param ? `#/${name}/${encodeURIComponent(param)}` : `#/${name}`;
}

export function navigate(name: RouteName, param?: string) {
  window.location.hash = href(name, param);
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseHash(window.location.hash));
  useEffect(() => {
    const on = () => setRoute(parseHash(window.location.hash));
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  return route;
}
