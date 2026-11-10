import { useId, type InputHTMLAttributes, type ReactNode } from "react";

export const inputClass =
  "h-8 w-full rounded-console border border-line-strong bg-surface px-2.5 text-[13px] text-fg placeholder:text-muted/70 focus:border-fg focus:outline-none aria-[invalid=true]:border-err";

/** Label + field + help or error. The error takes priority over the help. */
export function Field({
  label,
  hint,
  error,
  mono,
  ...props
}: InputHTMLAttributes<HTMLInputElement> & {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  mono?: boolean;
}) {
  const id = useId();
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={id} className="text-xs font-medium">
        {label}
      </label>
      <input
        id={id}
        aria-invalid={error ? true : undefined}
        aria-describedby={error || hint ? `${id}-help` : undefined}
        className={`${inputClass} ${mono ? "font-mono text-xs" : ""}`}
        {...props}
      />
      {(error || hint) && (
        <span id={`${id}-help`} className={`text-xs ${error ? "text-err" : "text-muted"}`}>
          {error ?? hint}
        </span>
      )}
    </div>
  );
}
