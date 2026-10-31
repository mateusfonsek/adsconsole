import * as RSwitch from "@radix-ui/react-switch";

export function Switch({
  checked,
  onCheckedChange,
  label,
  disabled,
}: {
  checked: boolean;
  onCheckedChange: (v: boolean) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <RSwitch.Root
      checked={checked}
      onCheckedChange={onCheckedChange}
      disabled={disabled}
      aria-label={label}
      className="relative h-[18px] w-8 shrink-0 rounded-[4px] bg-line-strong transition-colors data-[state=checked]:bg-fg disabled:opacity-40"
    >
      <RSwitch.Thumb className="block h-3.5 w-3.5 translate-x-0.5 rounded-[3px] bg-surface transition-transform data-[state=checked]:translate-x-[16px]" />
    </RSwitch.Root>
  );
}
