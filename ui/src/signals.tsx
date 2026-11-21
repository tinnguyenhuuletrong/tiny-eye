// Traces and Logs pages: filters that build TQS documents, rendered by the same PanelView as dashboards.
import { useMemo, useState } from "preact/hooks";
import type { Catalog, Panel, Tqs } from "./api";
import { PanelView } from "./panels";
import type { Range } from "./tqs";
import { Seg } from "./ui";

const panelOf = (query: Record<string, unknown>, ui: Panel["ui"]): Panel => ({ query: { tqs: "3", ...query } as Tqs, ui });

export function TracesPage({ catalog, range, tick }: { catalog?: Catalog; range: Range; tick: number }) {
  const [service, setService] = useState("");
  const [minMs, setMinMs] = useState(0);
  const [errors, setErrors] = useState(false);
  const [name, setName] = useState("");
  const search = useMemo(() => {
    const spans: Record<string, unknown>[] = [];
    if (service) spans.push({ field: "service_name", op: "=", value: service });
    if (name.trim()) spans.push({ field: "span_name", op: "contains", value: name.trim() });
    if (errors) spans.push({ field: "status_code", op: "=", value: 2 });
    const trace = minMs ? [{ field: "duration_ms", op: ">=", value: minMs }] : [];
    return panelOf({ id: "trace-search", kind: "trace_search", time: { from: "-1h" }, search: { spans, trace, order: "duration_desc" }, limit: 50 }, { display: "traces" });
  }, [service, minMs, errors, name]);
  const p95 = useMemo(() => panelOf({ id: "span-p95", kind: "timeseries", time: { from: "-1h" },
    queries: [{ name: "p95", spans: service ? [{ field: "service_name", op: "=", value: service }] : [], measure: { op: "quantile", field: "duration_ms", q: 0.95 }, agg: { by: ["service_name"] } }] }, { display: "timeseries", unit: "ms" }), [service]);
  return (
    <>
      <div class="toolbar"><div><div class="crumb">Traces</div><h1>Trace search</h1></div></div>
      <section class="view"><div class="grid">
        <div class="panel w12">
          <header><h2>Traces</h2><span class="sub">a trace matches when one of its spans matches every condition · click a row for the waterfall</span></header>
          <div class="body">
            <div class="filters" style={{ marginBottom: "12px" }}>
              <label class="select">service<select value={service} onChange={e => setService((e.target as HTMLSelectElement).value)}><option value="">any</option>{catalog?.services.map(s => <option>{s.name}</option>)}</select></label>
              <label class="select">span name contains<input class="var-input" value={name} onInput={e => setName((e.target as HTMLInputElement).value)} /></label>
              <label class="select">duration ≥<select value={minMs} onChange={e => setMinMs(+(e.target as HTMLSelectElement).value)}>{[0, 100, 500, 1000, 2000].map(v => <option value={v}>{v ? `${v} ms` : "any"}</option>)}</select></label>
              <label class="select"><input type="checkbox" checked={errors} onChange={e => setErrors((e.target as HTMLInputElement).checked)} /> errors only</label>
            </div>
            <PanelView panel={search} vars={{}} range={range} tick={tick} />
          </div>
        </div>
        <div class="panel w12" style={{ minHeight: "320px" }}>
          <header><h2>Span duration p95 by service</h2></header>
          <div class="body" style={{ height: "280px", display: "flex", flexDirection: "column" }}><PanelView panel={p95} vars={{}} range={range} tick={tick} sync="traces" /></div>
        </div>
      </div></section>
    </>
  );
}

export function LogsPage({ catalog, range, tick }: { catalog?: Catalog; range: Range; tick: number }) {
  const [text, setText] = useState("");
  const [sev, setSev] = useState<"all" | "warn" | "error">("all");
  const [service, setService] = useState("");
  const [tab, setTab] = useState<"lines" | "patterns">("lines");
  const conds = useMemo(() => {
    const c: Record<string, unknown>[] = [];
    if (text.trim()) c.push({ field: "body", op: "contains", value: text.trim() });
    if (sev !== "all") c.push({ field: "severity_number", op: ">=", value: sev === "warn" ? 13 : 17 });
    if (service) c.push({ field: "service_name", op: "=", value: service });
    return c;
  }, [text, sev, service]);
  const list = useMemo(() => panelOf(tab === "lines"
    ? { id: "log-search", kind: "log_search", time: { from: "-1h" }, search: { logs: conds, order: "desc" }, limit: 200 }
    : { id: "log-patterns", kind: "log_patterns", time: { from: "-1h" }, search: { logs: conds }, limit: 50 }, { display: "logs" }), [conds, tab]);
  const volume = useMemo(() => panelOf({ id: "log-volume", kind: "timeseries", time: { from: "-1h" },
    queries: [{ name: "n", logs: conds, measure: { op: "count" }, agg: { by: ["severity_text"] } }] }, { display: "bar" }), [conds]);
  const services = catalog?.services.filter(s => s.log_lines > 0) ?? [];
  return (
    <>
      <div class="toolbar"><div><div class="crumb">Logs</div><h1>Log search</h1></div></div>
      <section class="view"><div class="grid">
        <div class="panel w12">
          <header><h2>Volume</h2><span class="sub">lines per step by level</span></header>
          <div class="body" style={{ height: "170px", display: "flex", flexDirection: "column" }}><PanelView panel={volume} vars={{}} range={range} tick={tick} sync="logs" /></div>
        </div>
        <div class="panel w12">
          <div class="tabs" role="tablist">{(["lines", "patterns"] as const).map(t => <button type="button" role="tab" aria-selected={tab === t} class={tab === t ? "on" : ""} onClick={() => setTab(t)}>{t === "lines" ? "Lines" : "Patterns"}</button>)}</div>
          <div class="body" style={{ paddingTop: "12px" }}>
            <div class="filters" style={{ marginBottom: "12px" }}>
              <input class="input" style={{ maxWidth: "280px" }} placeholder="Message contains…" value={text} onInput={e => setText((e.target as HTMLInputElement).value)} aria-label="Message contains" />
              <Seg label="Level" value={sev} onChange={setSev} options={[["all", "all"], ["warn", "≥ warn"], ["error", "errors"]]} />
              <label class="select">service<select value={service} onChange={e => setService((e.target as HTMLSelectElement).value)}><option value="">any</option>{services.map(s => <option>{s.name}</option>)}</select></label>
            </div>
            <PanelView panel={list} vars={{}} range={range} tick={tick} />
            {tab === "patterns" && <p class="hint">Patterns group lines whose text matches after a fixed masking of numbers, ids, IPs and quoted strings, so a template groups the same on every node.</p>}
          </div>
        </div>
      </div></section>
    </>
  );
}
