// A dashboard: its YAML file is the source of truth. The page displays it, lets you edit layout
// and add panels, and saves the whole new state with PUT /api/v1/dashboards/{id}.
import { useEffect, useMemo, useRef, useState } from "preact/hooks";
import { api, ApiError, type Catalog, type Dashboard, type Panel, type Problem } from "./api";
import { PanelView } from "./panels";
import { FNS, displayOf, docFor, pickable, sizeOf, type Builder, type Range } from "./tqs";
import { Drawer, Empty, Problems, Seg, toast, useAsync } from "./ui";

export function DashboardPage({ id, range, tick, catalog, onSaved }: { id: string; range: Range; tick: number; catalog?: Catalog; onSaved: () => void }) {
  const res = useAsync(() => api.dashboard(id), [id]);
  const [saved, setSaved] = useState<Dashboard | null>(null);
  const [draft, setDraft] = useState<Dashboard | null>(null); // non-null while editing layout
  const [vars, setVars] = useState<Record<string, string>>({});
  const [problems, setProblems] = useState<Problem[]>([]);
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (res.data) { setSaved(res.data); setVars(res.data.vars ?? {}); setDraft(null); } }, [res.data]);

  if (!saved) return res.error ? <div class="view"><Problems error={res.error} /></div> : <div class="view"><Empty>Loading dashboard…</Empty></div>;
  const shown = draft ?? saved;

  const save = async (next: Dashboard, message: string) => {
    setBusy(true); setProblems([]);
    try {
      const d = await api.saveDashboard(next);
      setSaved(d); setDraft(null); toast(message); onSaved();
      return true;
    } catch (e) {
      setProblems(e instanceof ApiError && e.problems.length ? e.problems : [{ code: "error", path: "$", message: e instanceof Error ? e.message : String(e) }]);
      return false;
    } finally { setBusy(false); }
  };

  return (
    <>
      <div class="toolbar">
        <div><div class="crumb">Dashboards · {saved.id}.yaml</div><h1>{saved.title}</h1></div>
        <span class="spacer" />
        {Object.keys(saved.vars ?? {}).map(k => (
          <label class="select" key={k}>{k}<input class="var-input" value={vars[k]} onChange={e => setVars({ ...vars, [k]: (e.target as HTMLInputElement).value })} aria-label={`variable ${k}`} /></label>
        ))}
        {!draft && <><button class="btn" type="button" onClick={() => { setDraft(structuredClone(saved)); setProblems([]); }}>Edit layout</button>
          <button class="btn primary" type="button" onClick={() => setAdding(true)}>+ Add panel</button></>}
      </div>
      <section class="view">
        {draft && (
          <div class="edit-bar">
            <span><b>Editing layout.</b> Drag a panel to move it, drag its corner to resize, × removes it. Arrow keys work on the grip and the corner.</span>
            <span class="spacer" />
            <button class="btn" type="button" onClick={() => { setDraft(null); setProblems([]); }}>Cancel</button>
            <button class="btn primary" type="button" disabled={busy} onClick={() => save(draft, `Saved ${saved.id}.yaml`)}>{busy ? "Saving…" : "Done"}</button>
          </div>
        )}
        {problems.length > 0 && <div class="panel" style={{ padding: "12px 14px", marginBottom: "14px" }}><b style={{ fontSize: "13px" }}>The engine refused this state</b><Problems problems={problems} /></div>}
        <Board dash={shown} editing={!!draft} onChange={setDraft} vars={vars} range={range} tick={tick} onAdd={() => setAdding(true)} />
        {shown.panels.length === 0 && <Empty>This dashboard has no panels yet. Use “+ Add panel”.</Empty>}
      </section>
      {adding && <AddPanel catalog={catalog} range={range} vars={vars} onClose={() => setAdding(false)}
        onAdd={async p => { if (await save({ ...saved, panels: [...saved.panels, p] }, `Added “${p.query.title ?? p.query.id}” to ${saved.id}.yaml`)) setAdding(false); }} />}
    </>
  );
}

function Board({ dash, editing, onChange, vars, range, tick, onAdd }: { dash: Dashboard; editing: boolean; onChange: (d: Dashboard) => void; vars: Record<string, string>; range: Range; tick: number; onAdd: () => void }) {
  const grid = useRef<HTMLDivElement>(null);
  const [dragFrom, setDragFrom] = useState<number | null>(null);
  const [dropAt, setDropAt] = useState<{ i: number; before: boolean } | null>(null);
  const panels = dash.panels;
  const update = (ps: Panel[]) => onChange({ ...dash, panels: ps });
  const move = (from: number, to: number) => { const ps = [...panels]; const [p] = ps.splice(from, 1); ps.splice(Math.max(0, Math.min(ps.length, to)), 0, p); update(ps); };
  const resize = (i: number, w: number, h: number) => {
    w = Math.max(1, Math.min(12, w)); h = Math.max(1, Math.min(8, h));
    const cur = sizeOf(panels[i]);
    if (cur.w === w && cur.h === h) return;
    update(panels.map((p, k) => (k === i ? { ...p, ui: { ...p.ui, w, h } } : p)));
  };
  const startResize = (i: number, e: PointerEvent) => {
    e.preventDefault(); e.stopPropagation();
    const g = grid.current!, gap = parseFloat(getComputedStyle(g).columnGap) || 14, colW = (g.clientWidth - gap * 11) / 12, rowH = 92;
    const cell = (e.target as HTMLElement).closest(".panel")!.getBoundingClientRect();
    const handle = e.target as HTMLElement;
    handle.setPointerCapture(e.pointerId);
    const onMove = (ev: PointerEvent) => resize(i, Math.round((ev.clientX - cell.left + gap) / (colW + gap)), Math.round((ev.clientY - cell.top + gap) / (rowH + gap)));
    const onUp = () => { handle.removeEventListener("pointermove", onMove); handle.removeEventListener("pointerup", onUp); };
    handle.addEventListener("pointermove", onMove); handle.addEventListener("pointerup", onUp);
  };
  return (
    <div class={`grid board${editing ? " editing" : ""}`} ref={grid}>
      {panels.map((p, i) => {
        const { w, h } = sizeOf(p);
        const title = p.query.title ?? p.query.id;
        const drop = dropAt?.i === i ? (dropAt.before ? " drop-before" : " drop-after") : "";
        return (
          <div key={`${p.query.id}-${i}`} class={`panel w${w}${displayOf(p) === "stat" ? " stat" : ""}${dragFrom === i ? " dragging" : ""}${drop}`} style={{ "--h": h }}
            draggable={editing}
            onDragStart={e => { setDragFrom(i); e.dataTransfer?.setData("text/plain", String(i)); }}
            onDragEnd={() => { setDragFrom(null); setDropAt(null); }}
            onDragOver={e => { if (dragFrom === null || dragFrom === i) return; e.preventDefault(); const r = (e.currentTarget as HTMLElement).getBoundingClientRect(); setDropAt({ i, before: e.clientX < r.left + r.width / 2 }); }}
            onDrop={e => { e.preventDefault(); if (dragFrom === null || !dropAt) return; const to = dropAt.i + (dropAt.before ? 0 : 1); move(dragFrom, dragFrom < to ? to - 1 : to); setDragFrom(null); setDropAt(null); }}>
            <header>
              {editing && <span class="grip" role="button" tabIndex={0} aria-label={`Move ${title} (arrow keys)`} title="Drag to move"
                onKeyDown={e => { if (e.key === "ArrowLeft" || e.key === "ArrowUp") { e.preventDefault(); move(i, i - 1); } if (e.key === "ArrowRight" || e.key === "ArrowDown") { e.preventDefault(); move(i, i + 1); } }}>
                <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor" aria-hidden="true"><circle cx="5" cy="3.5" r="1.3" /><circle cx="11" cy="3.5" r="1.3" /><circle cx="5" cy="8" r="1.3" /><circle cx="11" cy="8" r="1.3" /><circle cx="5" cy="12.5" r="1.3" /><circle cx="11" cy="12.5" r="1.3" /></svg>
              </span>}
              <h2>{title}</h2>
              <div class="tools">
                <PanelSource panel={p} />
                {editing && <button class="icon-btn" type="button" title="Remove panel" aria-label={`Remove ${title}`} onClick={() => update(panels.filter((_, k) => k !== i))}>×</button>}
              </div>
            </header>
            <div class="body"><PanelView panel={p} vars={vars} range={range} tick={tick} sync={dash.id} /></div>
            {editing && <>
              <span class="size-tag">{w}×{h}</span>
              <button class="resize" type="button" aria-label={`Resize ${title} (arrow keys)`} title="Drag to resize" onPointerDown={e => startResize(i, e)}
                onKeyDown={e => { const d = ({ ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] } as Record<string, number[]>)[e.key]; if (d) { e.preventDefault(); resize(i, w + d[0], h + d[1]); } }} />
            </>}
          </div>
        );
      })}
      {editing && <button type="button" class="add-tile" onClick={onAdd}><span style={{ fontSize: "18px" }}>+</span> Add panel</button>}
    </div>
  );
}

function PanelSource({ panel }: { panel: Panel }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button class="icon-btn" type="button" title="Show the panel as stored" onClick={() => setOpen(true)}>{"{ }"}</button>
      {open && <Drawer title={`${panel.query.title ?? panel.query.id} · stored panel`} onClose={() => setOpen(false)}>
        <p class="hint" style={{ marginTop: 0 }}>As stored in the dashboard file: <span class="mono">query</span> is the TQS document, <span class="mono">ui</span> is how and where it is drawn.</p>
        <pre class="code">{JSON.stringify(panel, null, 2)}</pre>
      </Drawer>}
    </>
  );
}

function AddPanel({ catalog, range, vars, onClose, onAdd }: { catalog?: Catalog; range: Range; vars: Record<string, string>; onClose: () => void; onAdd: (p: Panel) => void }) {
  const metrics = useMemo(() => (catalog ? pickable(catalog) : []), [catalog]);
  const [b, setB] = useState<Builder>(() => ({ metric: metrics[0]?.name ?? "", fn: "", window: "5m", agg: "sum", by: [], filters: [] }));
  const [title, setTitle] = useState("");
  const [display, setDisplay] = useState<"timeseries" | "bar" | "stat">("timeseries");
  const [w, setW] = useState(6);
  const [pid] = useState(() => `panel-${Date.now().toString(36)}`);
  const m = metrics.find(x => x.name === b.metric);
  const fns = m ? FNS[m.type] : [];
  const fn = fns.some(([f]) => f === b.fn) ? b.fn : fns[0]?.[0] ?? "last";
  const builder = { ...b, fn, by: b.by.filter(l => m?.labels.includes(l)) };
  const auto = m ? `${fn} ${m.name}${builder.by.length ? " by " + builder.by.join(", ") : ""}` : "";
  const panel: Panel | null = m ? {
    query: { ...docFor(builder, m, range, display === "stat" ? "table" : "timeseries"), id: pid, title: title.trim() || auto } as Panel["query"],
    ui: { display, w, h: display === "stat" ? 2 : 4, ...(m.type === "histogram" ? { unit: "s" } : {}) },
  } : null;
  if (!catalog) return <Drawer title="New panel" onClose={onClose}><Empty>Loading the catalog…</Empty></Drawer>;
  if (!metrics.length) return <Drawer title="New panel" onClose={onClose}><Empty>No metrics yet. Send some data first (see Getting started), then add a panel.</Empty></Drawer>;
  return (
    <Drawer title="New panel" onClose={onClose}>
      <div class="form-grid">
        <div class="field full"><label for="np-title">Title</label><input class="input" id="np-title" placeholder={auto} value={title} onInput={e => setTitle((e.target as HTMLInputElement).value)} /></div>
        <div class="field full"><label for="np-metric">Metric</label>
          <select id="np-metric" class="mono" value={b.metric} onChange={e => setB({ ...b, metric: (e.target as HTMLSelectElement).value, by: [] })}>
            {metrics.map(x => <option value={x.name}>{x.name} ({x.type})</option>)}
          </select></div>
        <div class="field"><label for="np-fn">Function</label>
          <select id="np-fn" value={fn} onChange={e => setB({ ...b, fn: (e.target as HTMLSelectElement).value })}>{fns.map(([f, d]) => <option value={f}>{f} · {d}</option>)}</select></div>
        <div class="field"><label for="np-by">Group by</label>
          <select id="np-by" value={builder.by[0] ?? ""} onChange={e => { const v = (e.target as HTMLSelectElement).value; setB({ ...b, by: v ? [v] : [] }); }}>
            <option value="">nothing (one series)</option>{m?.labels.map(l => <option value={l}>{l}</option>)}
          </select></div>
        <div class="field"><span class="lbl">Visualization</span><Seg label="Visualization" value={display} onChange={v => { setDisplay(v); if (v === "stat" && w > 6) setW(3); }} options={[["timeseries", "Line"], ["bar", "Bars"], ["stat", "Stat"]]} /></div>
        <div class="field"><span class="lbl">Width</span><Seg label="Width" value={w} onChange={setW} options={[[3, "¼"], [6, "½"], [9, "¾"], [12, "Full"]]} /></div>
      </div>
      <div class="field" style={{ marginTop: "16px" }}><span class="lbl">Preview</span>
        <div class="np-preview"><b style={{ fontSize: "13.5px" }}>{title.trim() || auto}</b>{panel && <div style={{ height: display === "stat" ? "120px" : "240px", display: "flex", flexDirection: "column" }}><PanelView panel={panel} vars={vars} range={range} tick={0} /></div>}</div></div>
      <p class="hint" style={{ marginTop: "10px" }}>Filters, formulas and several queries: build it in Explore, then add it from there.</p>
      <div class="np-actions"><button class="btn primary" type="button" disabled={!panel} onClick={() => panel && onAdd(panel)}>Add panel</button></div>
    </Drawer>
  );
}
