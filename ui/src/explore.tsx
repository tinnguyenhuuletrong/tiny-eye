// Explore: build a query with pills (Builder) or edit the TQS document (TQS). Both edit the same
// query; the engine validates the document (POST /api/v1/validate) and runs it.
import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import { api, ApiError, type Catalog, type DashboardSummary, type Panel, type Problem, type Tqs } from "./api";
import { TimeChart, seriesColor, type Line } from "./charts";
import { fmtVal } from "./format";
import { FNS, builderFor, docFor, pickable, type Builder, type Filter, type PickMetric, type Range } from "./tqs";
import { Empty, Pill, Problems, Seg, toast, useAsync } from "./ui";

type Viz = "timeseries" | "bar" | "stat";

export function ExplorePage({ catalog, range, dashboards, onSaved }: { catalog?: Catalog; range: Range; dashboards: DashboardSummary[]; onSaved: () => void }) {
  const metrics = useMemo(() => (catalog ? pickable(catalog) : []), [catalog]);
  const [b, setB] = useState<Builder | null>(null);
  const [mode, setMode] = useState<"builder" | "tqs">("builder");
  const [text, setText] = useState("");
  const [textDoc, setTextDoc] = useState<Tqs | null>(null); // last valid document typed in TQS mode
  const [problems, setProblems] = useState<Problem[]>([]);
  const [reason, setReason] = useState<string | null>(null);
  const [viz, setViz] = useState<Viz>("timeseries");
  useEffect(() => {
    if (!b && metrics.length) {
      const m = metrics.find(x => x.type === "counter") ?? metrics[0];
      setB({ metric: m.name, fn: FNS[m.type][0][0], window: "5m", agg: "sum", by: m.labels.includes("job") ? ["job"] : [], filters: [] });
    }
  }, [metrics, b]);

  const m = metrics.find(x => x.name === b?.metric);
  const builderDoc = b && m ? docFor(b, m, range, viz === "stat" ? "table" : "timeseries") : null;
  const doc = mode === "tqs" ? textDoc : reason ? textDoc : builderDoc;

  // TQS editor: validate as you type, run when valid, sync back to the builder when it can show it.
  const timer = useRef<number | undefined>(undefined);
  const seq = useRef(0);
  const onText = (t: string) => {
    setText(t);
    clearTimeout(timer.current);
    const mine = ++seq.current;
    timer.current = window.setTimeout(async () => {
      let parsed: Tqs;
      try { parsed = JSON.parse(t); } catch (e) { setProblems([{ code: "schema", path: "$", message: `not JSON: ${(e as Error).message}` }]); return; }
      try {
        const v = await api.validate(t);
        if (mine !== seq.current) return; // a newer edit is being validated
        setProblems(v.problems);
        if (v.valid) {
          setTextDoc(parsed);
          const r = builderFor(parsed as Tqs & Record<string, unknown>, metrics);
          if (r.state) { setB(r.state); setReason(null); } else setReason(r.reason ?? null);
        }
      } catch (e) { if (mine === seq.current) setProblems(e instanceof ApiError ? e.problems : [{ code: "error", path: "$", message: String(e) }]); }
    }, 300);
  };
  const switchMode = (next: "builder" | "tqs") => {
    if (next === "tqs" && mode !== "tqs") {
      const d = reason && textDoc ? textDoc : builderDoc;
      setText(JSON.stringify(d, null, 2)); setTextDoc(d); setProblems([]);
    }
    setMode(next);
  };
  const editBuilder = (next: Builder) => { setB(next); setReason(null); };

  if (!catalog) return <div class="view"><Empty>Loading the catalog…</Empty></div>;
  if (!metrics.length) return <div class="view"><Empty>No metrics yet. Send some data (see Getting started) and they will show up here.</Empty></div>;

  return (
    <>
      <div class="toolbar"><div><div class="crumb">Explore</div><h1>Build a view</h1></div></div>
      <section class="view"><div class="ex-stack">
        <div class="panel qb">
          <div class="qb-head">
            <Seg label="Query editor" value={mode} onChange={switchMode} options={[["builder", "Builder"], ["tqs", "TQS"]]} />
            <span class="hint">{mode === "tqs" ? "Edit the document directly; it is validated by the engine as you type and runs when valid."
              : reason ? `This query uses ${reason}, which the builder cannot show. Switch to TQS to edit it.` : "Pick a metric, then refine it."}</span>
          </div>
          {mode === "builder" && b && m && <QueryBar b={b} m={m} metrics={metrics} onChange={editBuilder} />}
          {mode === "tqs" && (
            <div class="code-edit">
              <textarea spellcheck={false} value={text} aria-label="TQS document" onInput={e => onText((e.target as HTMLTextAreaElement).value)}
                onKeyDown={e => { if (e.key === "Tab" && !e.shiftKey) { e.preventDefault(); const t = e.target as HTMLTextAreaElement; t.setRangeText("  ", t.selectionStart, t.selectionEnd, "end"); onText(t.value); } }} />
              {problems.length ? <Problems problems={problems} /> : <div class="status-line"><span class="dot" style={{ background: "var(--good)" }} />Valid{reason ? ` · uses ${reason}` : ""}</div>}
            </div>
          )}
          {doc && <SaveTo doc={doc} viz={viz} dashboards={dashboards} onSaved={onSaved} />}
        </div>
        {doc && <Preview doc={doc} viz={viz} setViz={setViz} unit={m?.type === "histogram" ? "s" : undefined} />}
        {mode === "builder" && doc && <details class="panel tqs-box"><summary><span class="chev" /><h2>TQS document</h2><span class="sub">what this view stores and sends to POST /api/v1/query</span></summary><div class="body"><pre class="code">{JSON.stringify(doc, null, 2)}</pre></div></details>}
      </div></section>
    </>
  );
}

function QueryBar({ b, m, metrics, onChange }: { b: Builder; m: PickMetric; metrics: PickMetric[]; onChange: (b: Builder) => void }) {
  const [q, setQ] = useState("");
  const fns = FNS[m.type];
  const fn = fns.some(([f]) => f === b.fn) ? b.fn : fns[0][0];
  const set = (patch: Partial<Builder>) => onChange({ ...b, fn, ...patch });
  return (
    <div class="qb-row">
      <Pill k="metric" v={m.name} mono>{close => (
        <>
          <div><span class="lbl">Metric · {metrics.length} known</span><input class="input" placeholder="Filter metrics" value={q} onInput={e => setQ((e.target as HTMLInputElement).value)} autoFocus /></div>
          <div class="opt-list" role="listbox">{metrics.filter(x => x.name.includes(q.trim())).map(x => (
            <button type="button" class={`opt${x.name === m.name ? " on" : ""}`} onClick={() => { const t = FNS[x.type][0][0]; onChange({ ...b, metric: x.name, fn: t, by: b.by.filter(l => x.labels.includes(l)), filters: b.filters.filter(f => x.labels.includes(f.label)) }); close(); }}>
              <span class="name">{x.name}</span><span class="meta"><span class="badge">{x.type}</span> {x.series} series</span>
            </button>))}</div>
        </>
      )}</Pill>
      <Pill k={m.type === "histogram" ? "quantile" : "function"} v={`${fn}${fn === "last" ? "" : ` · ${b.window}`}`}>{close => (
        <>
          <div><span class="lbl">{m.type === "histogram" ? "Quantile" : "Function"}</span><div class="opt-list" style={{ maxHeight: "none" }}>
            {fns.map(([f, d]) => <button type="button" class={`opt${f === fn ? " on" : ""}`} onClick={() => { set({ fn: f }); close(); }}><span class="name">{f}</span><span class="meta">{d}</span></button>)}
          </div></div>
          <div><span class="lbl">Window</span><Seg label="Window" value={b.window} onChange={v => set({ window: v })} options={["1m", "2m", "5m", "15m", "1h"].map(w => [w, w] as [string, string])} /></div>
        </>
      )}</Pill>
      <span class="qb-word">aggregate</span>
      <Pill v={<>{b.agg}{b.by.length ? <> <span class="qb-word">by</span> <span class="m">{b.by.join(", ")}</span></> : <span class="qb-word"> of all series</span>}</>}>{() => (
        <>
          <div><span class="lbl">Aggregate</span><Seg label="Aggregate" value={b.agg} onChange={v => set({ agg: v })} options={["sum", "avg", "max", "min", "count"].map(a => [a, a] as [string, string])} /></div>
          <div><span class="lbl">Group by</span>{m.labels.map(l => (
            <label class="check"><input type="checkbox" checked={b.by.includes(l)} onChange={e => set({ by: (e.target as HTMLInputElement).checked ? [...b.by, l] : b.by.filter(x => x !== l) })} /> {l}</label>
          ))}</div>
        </>
      )}</Pill>
      {b.filters.length > 0 && <span class="qb-word">where</span>}
      {b.filters.map((f, i) => (
        <Pill v={<span class="m">{f.label} {f.op} {f.value}</span>} onRemove={() => set({ filters: b.filters.filter((_, k) => k !== i) })}>{close => (
          <FilterEditor m={m} initial={f} onApply={nf => { set({ filters: b.filters.map((x, k) => (k === i ? nf : x)) }); close(); }} onCancel={close} />
        )}</Pill>
      ))}
      <Pill v="+ Filter" ghost>{close => <FilterEditor m={m} onApply={nf => { set({ filters: [...b.filters, nf] }); close(); }} onCancel={close} />}</Pill>
    </div>
  );
}

function FilterEditor({ m, initial, onApply, onCancel }: { m: PickMetric; initial?: Filter; onApply: (f: Filter) => void; onCancel: () => void }) {
  const [f, setF] = useState<Filter>(initial ?? { label: m.labels[0] ?? "", op: "=", value: m.values[m.labels[0]]?.[0] ?? "" });
  const values = m.values[f.label] ?? [];
  const regex = f.op.includes("~");
  return (
    <>
      <div><span class="lbl">{initial ? "Edit filter" : "Add filter"}</span><div class="pop-grid">
        <select aria-label="Label" value={f.label} onChange={e => { const l = (e.target as HTMLSelectElement).value; setF({ ...f, label: l, value: m.values[l]?.[0] ?? "" }); }}>{m.labels.map(l => <option>{l}</option>)}</select>
        <select aria-label="Operator" value={f.op} onChange={e => setF({ ...f, op: (e.target as HTMLSelectElement).value as Filter["op"] })}>{["=", "!=", "=~", "!~"].map(o => <option>{o}</option>)}</select>
        {regex || !values.length
          ? <input aria-label="Value" value={f.value} placeholder={regex ? "regex, fully anchored" : "value"} onInput={e => setF({ ...f, value: (e.target as HTMLInputElement).value })} />
          : <select aria-label="Value" value={f.value} onChange={e => setF({ ...f, value: (e.target as HTMLSelectElement).value })}>{values.map(v => <option>{v}</option>)}</select>}
      </div></div>
      <div class="pop-actions"><button class="btn" type="button" onClick={onCancel}>Cancel</button><button class="btn primary" type="button" onClick={() => onApply(f)}>{initial ? "Apply" : "Add filter"}</button></div>
    </>
  );
}

function Preview({ doc, viz, setViz, unit }: { doc: Tqs; viz: Viz; setViz: (v: Viz) => void; unit?: string }) {
  const key = JSON.stringify(doc);
  const res = useAsync(() => api.query(doc), [key]);
  const [hidden, setHidden] = useState<Set<number>>(new Set());
  useEffect(() => setHidden(new Set()), [key]);
  const series = res.data?.series ?? [];
  const lines: Line[] = series.map((s, i) => ({ label: s.key || s.output, points: s.points as [number, number][], color: i })).filter((_, i) => !hidden.has(i));
  const stat = (vals: number[], f: (v: number[]) => number) => (vals.length ? fmtVal(f(vals), unit) : "–");
  return (
    <div class="panel">
      <header><h2>Result</h2><span class="sub">{series.length} series{res.data ? ` · ${Math.round(res.data.elapsed_ms)} ms` : ""}</span>
        <div class="tools"><Seg label="Visualization" value={viz} onChange={setViz} options={[["timeseries", "line"], ["bar", "bars"], ["stat", "stat"]]} /></div></header>
      <div class="body" style={{ minHeight: "320px", display: "flex", flexDirection: "column" }}>
        {res.error ? <Problems error={res.error} /> : !res.data ? <Empty>Running…</Empty> : !series.length ? <Empty>No data in this range.</Empty>
          : viz === "stat" ? <div class="stat-grid">{series.map((s, i) => <div class="stat-card"><div class="hint mono">{s.key || s.output}</div><div class="num big">{fmtVal(s.points[s.points.length - 1]?.[1] as number, unit)}</div><i style={{ background: seriesColor(i) }} /></div>)}</div>
          : <div style={{ height: "300px", display: "flex", flexDirection: "column" }}><TimeChart key={[...hidden].join(",")} lines={lines} unit={unit} bars={viz === "bar"} sync="explore" /></div>}
      </div>
      {series.length > 0 && <div class="ex-table scroll-x"><table>
        <thead><tr><th>Series</th><th class="r">Last</th><th class="r">Min</th><th class="r">Max</th><th class="r">Avg</th></tr></thead>
        <tbody>{series.map((s, i) => { const v = s.points.map(p => p[1] as number); return (
          <tr class={`series-row${hidden.has(i) ? " off" : ""}`} tabIndex={0} onClick={() => setHidden(h => { const n = new Set(h); n.has(i) ? n.delete(i) : n.add(i); return n; })}>
            <td class="mono"><span class="sw" style={{ background: seriesColor(i) }} />{s.key || s.output}</td>
            <td class="r num">{stat(v, x => x[x.length - 1])}</td><td class="r num">{stat(v, x => Math.min(...x))}</td><td class="r num">{stat(v, x => Math.max(...x))}</td><td class="r num">{stat(v, x => x.reduce((a, c) => a + c, 0) / x.length)}</td>
          </tr>); })}</tbody>
      </table></div>}
    </div>
  );
}

function SaveTo({ doc, viz, dashboards, onSaved }: { doc: Tqs; viz: Viz; dashboards: DashboardSummary[]; onSaved: () => void }) {
  const [target, setTarget] = useState(dashboards[0]?.id ?? "");
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (!target && dashboards[0]) setTarget(dashboards[0].id); }, [dashboards]);
  if (!dashboards.length) return <div class="qb-foot"><span class="hint">Add a dashboard YAML file to the dashboards directory to save views.</span></div>;
  const add = async () => {
    setBusy(true);
    try {
      const d = await api.dashboard(target);
      const id = `panel-${Date.now().toString(36)}`;
      const panel: Panel = { query: { ...doc, id, title: title.trim() || undefined } as Panel["query"], ui: { display: viz, w: viz === "stat" ? 3 : 6, h: viz === "stat" ? 2 : 4 } };
      await api.saveDashboard({ ...d, panels: [...d.panels, panel] });
      toast(`Added to ${d.title} (${d.id}.yaml)`); onSaved();
    } catch (e) { toast(e instanceof ApiError ? e.message : String(e)); } finally { setBusy(false); }
  };
  return (
    <div class="qb-foot">
      <input class="input" style={{ maxWidth: "260px" }} placeholder="Panel title (optional)" value={title} onInput={e => setTitle((e.target as HTMLInputElement).value)} aria-label="Panel title" />
      <span class="spacer" />
      <label class="select">dashboard<select value={target} onChange={e => setTarget((e.target as HTMLSelectElement).value)}>{dashboards.map(d => <option value={d.id}>{d.title}</option>)}</select></label>
      <button class="btn primary" type="button" disabled={busy || !target} onClick={add}>{busy ? "Saving…" : "Add to dashboard"}</button>
    </div>
  );
}
