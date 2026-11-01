export type DiffRow = { path: string; before: unknown; after: unknown };

function flatten(v: unknown, prefix: string, out: Map<string, unknown>) {
  if (v && typeof v === "object" && !Array.isArray(v)) {
    for (const [k, inner] of Object.entries(v)) flatten(inner, prefix ? `${prefix}.${k}` : k, out);
  } else if (prefix) {
    out.set(prefix, v);
  }
}

/** Campos que mudaram entre `before` e `after`, achatados como `a.b.c`, em ordem alfabética. */
export function diffRows(before: unknown, after: unknown): DiffRow[] {
  const b = new Map<string, unknown>();
  const a = new Map<string, unknown>();
  flatten(before, "", b);
  flatten(after, "", a);
  const paths = [...new Set([...b.keys(), ...a.keys()])].sort();
  return paths
    .filter((p) => JSON.stringify(b.get(p)) !== JSON.stringify(a.get(p)))
    .map((p) => ({ path: p, before: b.get(p), after: a.get(p) }));
}

function show(v: unknown): string {
  if (v === undefined || v === null) return "—";
  return typeof v === "string" ? v : JSON.stringify(v);
}

export function ChangeDiff({
  before,
  after,
  labels,
}: {
  before: unknown;
  after: unknown;
  labels: { field: string; before: string; after: string; empty?: string };
}) {
  const rows = diffRows(before, after);
  if (!rows.length) return labels.empty ? <p className="text-xs text-muted">{labels.empty}</p> : null;
  return (
    <table className="w-full border-collapse text-xs">
      <thead>
        <tr className="border-b border-line">
          {[labels.field, labels.before, labels.after].map((h) => (
            <th key={h} className="label-mono py-2 pr-4 text-left font-medium">
              {h}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.path} className="border-b border-line/70 last:border-b-0">
            <td className="py-2 pr-4 font-mono text-[11.5px]">{r.path}</td>
            <td className="py-2 pr-4">
              <span className="font-mono text-[11.5px] text-muted line-through decoration-muted/60">{show(r.before)}</span>
            </td>
            <td className="py-2 font-mono text-[11.5px] font-medium">{show(r.after)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
