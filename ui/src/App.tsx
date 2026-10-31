import { useEffect, useState } from "react";
import { commands } from "./bindings";

export function App() {
  const [version, setVersion] = useState<string>();

  useEffect(() => {
    commands.appVersion().then(setVersion, () => setVersion(undefined));
  }, []);

  return (
    <main className="flex h-full flex-col items-center justify-center gap-1 bg-bg text-fg">
      <h1 className="text-xl font-semibold tracking-tight">AdsConsole</h1>
      {version && <span className="font-mono text-xs text-muted">{version}</span>}
    </main>
  );
}
