import type { ButtonHTMLAttributes } from "react";

type Variant = "primary" | "secondary" | "ghost" | "danger";

const styles: Record<Variant, string> = {
  primary: "bg-fg text-bg hover:opacity-90",
  secondary: "border border-line-strong bg-surface text-fg hover:bg-line/50",
  ghost: "text-muted hover:text-fg hover:bg-line/60",
  danger: "border border-err/40 text-err hover:bg-err/10",
};

export function Button({
  variant = "secondary",
  size = "md",
  className = "",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; size?: "sm" | "md" }) {
  const pad = size === "sm" ? "h-7 px-2.5 text-xs" : "h-8 px-3.5 text-[13px]";
  return (
    <button
      type="button"
      className={`inline-flex items-center justify-center gap-1.5 rounded-console font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-fg disabled:pointer-events-none disabled:opacity-40 ${pad} ${styles[variant]} ${className}`}
      {...props}
    />
  );
}
