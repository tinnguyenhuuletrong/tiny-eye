// Number and time formatting shared by charts, stats and tables.

export function fmtVal(v: number | null | undefined, unit?: string): string {
  if (v == null || !Number.isFinite(v)) return "–";
  const u = unit ?? "";
  if (u === "s") return Math.abs(v) < 1 ? `${trim(v * 1000, v * 1000 < 10 ? 1 : 0)} ms` : `${trim(v, 2)} s`;
  if (u === "ms") return Math.abs(v) < 1000 ? `${Math.round(v)} ms` : `${trim(v / 1000, 2)} s`;
  if (u === "%") return `${trim(v, v < 10 ? 2 : 1)}%`;
  if (u === "bytes") return bytes(v);
  const n = Math.abs(v) >= 1e6 ? `${trim(v / 1e6, 1)}M` : Math.abs(v) >= 1e4 ? `${trim(v / 1e3, 1)}k` : trim(v, Math.abs(v) < 10 ? 2 : 1);
  return u ? `${n} ${u}` : n;
}
function trim(v: number, d: number): string { return String(+v.toFixed(d)); }
function bytes(v: number): string {
  const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0;
  while (Math.abs(v) >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${trim(v, v < 10 ? 1 : 0)} ${u[i]}`;
}
export function fmtTime(ms: number, sec = false): string {
  const d = new Date(ms), p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}${sec ? ":" + p(d.getSeconds()) : ""}`;
}
export function fmtDateTime(ms: number): string {
  return new Date(ms).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
/** Highlight `<*>` placeholders of a log pattern. */
export function maskParts(s: string): { text: string; mask: boolean }[] {
  return s.split(/(<\*>)/).filter(Boolean).map(t => ({ text: t, mask: t === "<*>" }));
}
