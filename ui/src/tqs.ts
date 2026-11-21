// TQS helpers: the dashboard's context applied to a stored panel query, display inference, and the
// Explore builder's state <-> document mapping. The engine stays the validator of record.
import type { Catalog, CatalogMetric, Panel, PanelUi, QueryResponse, Tqs } from "./api";

export const RANGES = ["15m", "1h", "6h", "24h", "7d"] as const;
export type Range = (typeof RANGES)[number];

type Doc = Tqs & Record<string, unknown>;

/** What a panel runs: its stored query with dashboard vars (defaults) and the picked range.
 *  A stat over metric queries runs as a time series: the same latest value, plus a sparkline.
 *  (Spans/logs stats keep `table`: there the value is a total over the range.) */
export function effectiveQuery(q: Tqs, vars: Record<string, string>, range: Range, display?: string): Tqs {
  const d = structuredClone(q) as Doc;
  const v = { ...vars, ...((d.vars as Record<string, string>) ?? {}) };
  if (Object.keys(v).length) d.vars = v;
  if (d.kind !== "trace") d.time = { from: `-${range}` }; // engine picks the step (~300 points)
  const qs = (d.queries as { metric?: string }[] | undefined) ?? [];
  if (display === "stat" && d.kind === "table" && qs.length && qs.every(x => x.metric)) d.kind = "timeseries";
  return d;
}

export type Display = NonNullable<PanelUi["display"]>;
export function displayOf(p: Panel, resp?: QueryResponse): Display {
  if (p.ui?.display) return p.ui.display;
  switch (p.query.kind) {
    case "timeseries": return "timeseries";
    case "trace_search": case "trace": return "traces";
    case "log_search": case "log_patterns": return "logs";
    default: return resp && resp.series.length > 1 ? "table" : "stat";
  }
}
export function sizeOf(p: Panel): { w: number; h: number } {
  const stat = displayOf(p) === "stat";
  return { w: p.ui?.w ?? (stat ? 3 : 6), h: p.ui?.h ?? (stat ? 2 : 4) };
}

// ---------------------------------------------------------------- Explore builder
export type MetricType = "counter" | "gauge" | "histogram";
export interface PickMetric { name: string; type: MetricType; labels: string[]; values: Record<string, string[]>; series: number }
export interface Filter { label: string; op: "=" | "!=" | "=~" | "!~"; value: string }
export interface Builder { metric: string; fn: string; window: string; agg: string; by: string[]; filters: Filter[] }

export const FNS: Record<MetricType, [string, string][]> = {
  counter: [["rate", "per-second rate"], ["increase", "increase over the window"], ["last", "latest value"]],
  gauge: [["last", "latest value"], ["avg_over_time", "average over the window"], ["max_over_time", "maximum over the window"], ["min_over_time", "minimum over the window"]],
  histogram: [["p50", "median"], ["p90", "90th percentile"], ["p95", "95th percentile"], ["p99", "99th percentile"]],
};
const Q: Record<string, number> = { p50: 0.5, p90: 0.9, p95: 0.95, p99: 0.99 };

/** Metrics as the builder shows them: histograms by base name (from `_bucket`), types inferred
 *  from the name when the sender did not report one (remote-write). */
export function pickable(c: Catalog): PickMetric[] {
  const out = new Map<string, PickMetric>();
  const add = (m: CatalogMetric, name: string, type: MetricType) => {
    const labels = m.label_keys.filter(l => l !== "le");
    out.set(name, { name, type, labels, values: m.label_values as Record<string, string[]>, series: m.series });
  };
  for (const m of c.metrics) {
    if (m.name.endsWith("_bucket")) add(m, m.name.slice(0, -7), "histogram");
    else if (m.type === "counter" || m.type === "gauge") add(m, m.name, m.type);
    else add(m, m.name, /(_total|_count|_sum)$/.test(m.name) ? "counter" : "gauge");
  }
  return [...out.values()].sort((a, b) => a.name.localeCompare(b.name));
}

export function docFor(b: Builder, m: PickMetric | undefined, range: Range, kind: "timeseries" | "table" = "timeseries"): Tqs {
  const hist = m?.type === "histogram";
  const fn = hist ? { op: "histogram_quantile", q: Q[b.fn] ?? 0.95, window: b.window } : b.fn === "last" ? { op: "last" } : { op: b.fn, window: b.window };
  const query: Record<string, unknown> = { name: "a", metric: b.metric };
  if (b.filters.length) query.match = b.filters.map(f => ({ ...f }));
  query.fn = fn;
  query.agg = b.by.length ? { op: b.agg, by: [...b.by] } : { op: b.agg };
  return { tqs: "3", id: "explore", kind, time: { from: `-${range}` }, queries: [query] } as unknown as Tqs;
}

/** The builder state for a document, or why the builder cannot show it. */
export function builderFor(doc: Doc, metrics: PickMetric[]): { state?: Builder; reason?: string } {
  const qs = (doc.queries as Record<string, any>[] | undefined) ?? [];
  if (!["timeseries", "table"].includes(String(doc.kind))) return { reason: `${doc.kind} documents belong to the Traces and Logs pages` };
  if (qs.length !== 1) return { reason: "several queries" };
  if ((doc.formulas as unknown[] | undefined)?.length) return { reason: "formulas" };
  const q = qs[0];
  if (!q.metric) return { reason: "a spans or logs query" };
  if (q.then || q.derive || q.offset || q.having || q.fill) return { reason: "then / derive / offset / having / fill" };
  const m = metrics.find(x => x.name === q.metric);
  if (!m) return { reason: `${q.metric} is not in the catalog` };
  const fn = q.fn?.op === "histogram_quantile" ? Object.keys(Q).find(k => Q[k] === q.fn.q) : q.fn?.op;
  if (!fn || !FNS[m.type].some(([f]) => f === fn)) return { reason: `function ${q.fn?.op} ${q.fn?.q ?? ""}` };
  return { state: { metric: m.name, fn, window: q.fn.window ?? "5m", agg: q.agg?.op ?? "sum", by: q.agg?.by ?? [], filters: q.match ?? [] } };
}
