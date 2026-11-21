// One component that runs any panel's TQS query and draws it the way its `ui` says.
import { useMemo, useState } from "preact/hooks";
import { api, type Panel, type QueryResponse, type Tqs } from "./api";
import { Sparkline, TimeChart, type Line } from "./charts";
import { fmtTime, fmtVal } from "./format";
import { displayOf, effectiveQuery, type Range } from "./tqs";
import { Drawer, Empty, Problems, useAsync } from "./ui";

export function PanelView({ panel, vars, range, tick, sync }: { panel: Panel; vars: Record<string, string>; range: Range; tick: number; sync?: string }) {
  const q = useMemo(() => effectiveQuery(panel.query, vars, range, panel.ui?.display), [panel.query, panel.ui?.display, vars, range]);
  const key = JSON.stringify(q);
  const res = useAsync(() => api.query(q), [key, tick]);
  const [trace, setTrace] = useState<string | null>(null);
  const d = res.data;
  if (!d) return res.error ? <Problems error={res.error} /> : <Empty>Loading…</Empty>;
  const display = displayOf(panel, d);
  const unit = panel.ui?.unit;
  return (
    <>
      {res.error ? <Problems error={res.error} /> : null}
      {display === "stat" && <Stat d={d} unit={unit} />}
      {(display === "timeseries" || display === "bar") && <Chart d={d} unit={unit} bars={display === "bar"} sync={sync} />}
      {display === "table" && <ResultTable d={d} unit={unit} order={outputOrder(q)} />}
      {display === "logs" && <LogRows d={d} onTrace={setTrace} />}
      {display === "traces" && (q.kind === "trace" ? <Waterfall rows={d.rows} /> : <TraceRows d={d} onTrace={setTrace} />)}
      {trace && <TraceDrawer id={trace} onClose={() => setTrace(null)} />}
    </>
  );
}

/** Log levels are state, not identity: they take the reserved status colors. */
const LEVEL_COLOR: Record<string, string> = { FATAL: "--critical", ERROR: "--critical", WARN: "--warning", WARNING: "--warning", INFO: "--line-strong", DEBUG: "--line", TRACE: "--line" };
function linesOf(d: QueryResponse): Line[] {
  return d.series.map((s, i) => {
    const level = s.key.match(/^severity_text=(\w+)$/)?.[1]?.toUpperCase();
    return { label: s.key || s.output, points: s.points as [number, number][], color: i, css: level ? LEVEL_COLOR[level] : undefined };
  });
}

function Chart({ d, unit, bars, sync }: { d: QueryResponse; unit?: string; bars?: boolean; sync?: string }) {
  const lines = useMemo(() => linesOf(d), [d]);
  if (!lines.length) return <Empty>No data in this range.</Empty>;
  return <TimeChart lines={lines} unit={unit} bars={bars} sync={sync} />;
}

function Stat({ d, unit }: { d: QueryResponse; unit?: string }) {
  const s = d.series[0];
  if (!s) return <Empty>No data.</Empty>;
  const vals = s.points.map(p => p[1] as number), last = vals[vals.length - 1];
  const [num, u] = splitUnit(fmtVal(last, unit === "%" || unit === "s" ? unit : undefined), unit);
  return (
    <div class="stat-body">
      <div class="value num">{num}<span class="unit">{u}</span></div>
      <div class="meta">{d.series.length > 1 ? `${s.key || s.output} · ${d.series.length - 1} more series` : s.key || " "}</div>
      {vals.length > 1 && <Sparkline values={vals} />}
    </div>
  );
}
function splitUnit(text: string, unit?: string): [string, string] {
  const m = text.match(/^([^\s%]+)\s?(.*)$/);
  if (!m) return [text, unit ?? ""];
  return [m[1], m[2] || (text.endsWith("%") ? "" : unit && unit !== "s" ? unit : "")];
}

/** Output names in document order: queries, then formulas. */
function outputOrder(q: Tqs): string[] {
  const doc = q as Tqs & { queries?: { name: string }[]; formulas?: { name: string }[] };
  return [...(doc.queries ?? []).map(x => x.name), ...(doc.formulas ?? []).map(x => x.name)];
}

/** Table kinds pivot to one row per series key and one column per output; list kinds show rows. */
function ResultTable({ d, unit, order }: { d: QueryResponse; unit?: string; order: string[] }) {
  if (d.rows.length) return <RowTable rows={d.rows} />;
  const outputs = [...new Set(d.series.map(s => s.output))].sort((a, b) => order.indexOf(a) - order.indexOf(b));
  const keys = [...new Set(d.series.map(s => s.key))];
  const cell = (k: string, o: string) => { const s = d.series.find(x => x.key === k && x.output === o); return s ? (s.points[s.points.length - 1][1] as number) : null; };
  if (!keys.length) return <Empty>No data.</Empty>;
  return (
    <div class="scroll-x"><table>
      <thead><tr><th>Series</th>{outputs.map(o => <th class="r">{o}</th>)}</tr></thead>
      <tbody>{keys.map(k => <tr><td class="mono">{k || "(all)"}</td>{outputs.map(o => <td class="r num">{fmtVal(cell(k, o), unit)}</td>)}</tr>)}</tbody>
    </table></div>
  );
}

function RowTable({ rows }: { rows: Record<string, string>[] }) {
  const cols = Object.keys(rows[0] ?? {});
  return (
    <div class="scroll-x"><table>
      <thead><tr>{cols.map(c => <th>{c}</th>)}</tr></thead>
      <tbody>{rows.map(r => <tr>{cols.map(c => <td class={/^-?[\d.]+$/.test(r[c]) ? "r num" : ""}>{r[c]}</td>)}</tr>)}</tbody>
    </table></div>
  );
}

const SEV = (s: string) => (s.startsWith("ERR") || s === "FATAL" ? "ERROR" : s.startsWith("WARN") ? "WARN" : "INFO");

function LogRows({ d, onTrace }: { d: QueryResponse; onTrace: (id: string) => void }) {
  if (!d.rows.length) return <Empty>No lines match.</Empty>;
  if ("pattern_hash" in d.rows[0]) {
    return (
      <div class="scroll-x"><table>
        <thead><tr><th class="r">Lines</th><th>Example</th><th>Services</th></tr></thead>
        <tbody>{d.rows.map(r => <tr><td class="r num">{r.n}</td><td class="body-text">{r.example}</td><td class="mono">{r.services.replace(/[[\]]/g, "")}</td></tr>)}</tbody>
      </table></div>
    );
  }
  return (
    <div class="scroll-x"><table>
      <thead><tr><th>Time</th><th>Level</th><th>Service</th><th>Message</th></tr></thead>
      <tbody>{d.rows.map(r => (
        <tr class="log-row">
          <td class="num mono" style={{ whiteSpace: "nowrap", color: "var(--ink-3)" }}>{fmtTime(+r.ts_ms, true)}</td>
          <td><span class={`sev ${SEV(r.severity_text)}`}>{r.severity_text}</span></td>
          <td class="mono">{r.service_name}</td>
          <td class="body-text">{r.body}{r.trace_id && <button type="button" class="icon-btn" title="Open trace" onClick={() => onTrace(r.trace_id)}>trace</button>}</td>
        </tr>
      ))}</tbody>
    </table></div>
  );
}

function TraceRows({ d, onTrace }: { d: QueryResponse; onTrace: (id: string) => void }) {
  if (!d.rows.length) return <Empty>No traces match.</Empty>;
  const max = Math.max(...d.rows.map(r => +r.duration_ms), 1);
  return (
    <div class="scroll-x"><table>
      <thead><tr><th>Trace</th><th>Root span</th><th>Services</th><th class="r">Spans</th><th style={{ minWidth: "180px" }}>Duration</th></tr></thead>
      <tbody>{d.rows.map(r => {
        const err = r.has_error === "true";
        return (
          <tr class="click" tabIndex={0} onClick={() => onTrace(r.trace_id)} onKeyDown={e => e.key === "Enter" && onTrace(r.trace_id)}>
            <td class="mono">{r.trace_id.slice(0, 12)}… {err && <span class="err-badge">● error</span>}</td>
            <td>{r.root_service} · {r.root_span}</td>
            <td class="mono" style={{ color: "var(--ink-2)" }}>{r.services}</td>
            <td class="r num">{r.span_count}</td>
            <td><div class="dur"><div class="track"><div class="fill" style={{ width: `${(+r.duration_ms / max) * 100}%`, background: err ? "var(--critical)" : "var(--s1)" }} /></div><span class="num" style={{ width: "64px", textAlign: "right" }}>{fmtVal(+r.duration_ms, "ms")}</span></div></td>
          </tr>
        );
      })}</tbody>
    </table></div>
  );
}

export function TraceDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const res = useAsync(() => api.query({ tqs: "3", id: "waterfall", kind: "trace", trace_id: id } as Tqs), [id]);
  return (
    <Drawer title={`Trace ${id.slice(0, 16)}…`} onClose={onClose}>
      {res.data ? <Waterfall rows={res.data.rows} /> : res.error ? <Problems error={res.error} /> : <Empty>Loading…</Empty>}
    </Drawer>
  );
}

function Waterfall({ rows }: { rows: Record<string, string>[] }) {
  if (!rows.length) return <Empty>No spans for this trace id.</Empty>;
  const total = Math.max(...rows.map(r => +r.offset_ms + +r.duration_ms), 0.001);
  const services = [...new Set(rows.map(r => r.service_name))];
  const color = (s: string) => `var(--s${(services.indexOf(s) % 8) + 1})`;
  const errors = rows.filter(r => r.status_code === "2").length;
  return (
    <div>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "8px 18px", marginBottom: "12px", fontSize: "13px" }}>
        <span><b class="num">{fmtVal(total, "ms")}</b> total</span><span><b class="num">{rows.length}</b> spans</span>
        <span>{errors ? <span class="pill crit"><span class="dot" style={{ background: "var(--critical)" }} />{errors} error span{errors > 1 ? "s" : ""}</span> : <span class="pill good"><span class="dot" style={{ background: "var(--good)" }} />No errors</span>}</span>
      </div>
      <div style={{ marginBottom: "10px" }}>{services.map(s => <span style={{ display: "inline-flex", alignItems: "center", gap: "6px", marginRight: "12px", fontSize: "12px", color: "var(--ink-2)" }}><span class="svc" style={{ background: color(s) }} />{s}</span>)}</div>
      <div class="wf">
        <div />
        <div class="ticks">{[0, 0.25, 0.5, 0.75, 1].map(f => <span style={{ left: `${f * 100}%`, transform: f === 0 ? "none" : f === 1 ? "translateX(-100%)" : undefined }}>{fmtVal(total * f, "ms")}</span>)}</div>
        {rows.map(r => { const end = (+r.offset_ms + +r.duration_ms) / total; return (
          <>
            <div class="name" style={{ paddingLeft: `${+r.depth * 14}px` }} title={`self ${r.self_ms} ms${r.orphan === "true" ? " · parent missing" : ""}`}><span class="svc" style={{ background: color(r.service_name) }} />{r.span_name}</div>
            <div class="lane"><div class="span" style={{ left: `${(+r.offset_ms / total) * 100}%`, width: `${Math.max(0.3, (+r.duration_ms / total) * 100)}%`, background: r.status_code === "2" ? "var(--critical)" : color(r.service_name) }}><em class={`num${end > 0.75 ? " inside" : ""}`}>{fmtVal(+r.duration_ms, "ms")}</em></div></div>
          </>
        ); })}
      </div>
    </div>
  );
}
