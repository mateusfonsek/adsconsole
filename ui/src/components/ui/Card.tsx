import type { HTMLAttributes, ReactNode } from "react";

export function Card({ className = "", ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={`rounded-console border border-line bg-surface ${className}`} {...props} />;
}

/** Mono text block with a background, for configuration snippets. */
export function CodeBlock({ children, action }: { children: string; action?: ReactNode }) {
  return (
    <div className="relative rounded-console border border-line bg-line/40">
      <pre className="overflow-x-auto p-3 pr-20 font-mono text-[11.5px] leading-relaxed text-fg">{children}</pre>
      {action && <div className="absolute right-2 top-2">{action}</div>}
    </div>
  );
}

/** Inline error message, with the error code in subdued mono. */
export function ErrorNote({ message, code }: { message: string; code?: string }) {
  return (
    <div role="alert" className="rounded-console border border-err/30 bg-err/5 px-3 py-2 text-xs text-err">
      {message}
      {code && <span className="ml-2 font-mono text-[10.5px] opacity-70">{code}</span>}
    </div>
  );
}
