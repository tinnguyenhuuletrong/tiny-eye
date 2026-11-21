// Time-series charts (uPlot on canvas) with a crosshair synced across a page, and sparklines.
import { useEffect, useRef, useState } from "preact/hooks";
import uPlot from "uplot";
import { fmtDateTime, fmtTime, fmtVal } from "./format";

/** `color` indexes the categorical palette; `css` names a token instead (status colors for levels). */
export interface Line { label: string; points: [number, number][]; color: number; css?: string }

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
export const seriesColor = (i: number) => css(`--s${(i % 8) + 1}`);
const lineColor = (l: Line) => (l.css ? css(l.css) : seriesColor(l.color));

/** Re-render when the system theme flips (canvas colors are read once per build). */
function useThemeVersion() {
  const [v, setV] = useState(0);
  useEffect(() => {
    const mq = matchMedia("(prefers-color-scheme: dark)");
    const on = () => setV(x => x + 1);
    mq.addEventListener("change", on);
    return () => mq.removeEventListener("change", on);
  }, []);
  return v;
}

export function TimeChart({ lines, unit, bars, sync = "page", threshold }: { lines: Line[]; unit?: string; bars?: boolean; sync?: string; threshold?: number }) {
  const box = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const plot = useRef<uPlot | null>(null);
  const [hidden, setHidden] = useState<Set<number>>(new Set());
  const theme = useThemeVersion();

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const xs = [...new Set(lines.flatMap(l => l.points.map(p => p[0])))].sort((a, b) => a - b);
    const idx = new Map(xs.map((x, i) => [x, i]));
    const raw = lines.map(l => { const a: (number | null)[] = new Array(xs.length).fill(null); l.points.forEach(([t, v]) => { a[idx.get(t)!] = v; }); return a; });
    // Several bar series stack: series k is drawn up to the sum of series 0..k (visible ones), and
    // drawn from the top of the stack down so every segment shows. The tooltip reads `raw`.
    const stack = !!bars && lines.length > 1;
    const ys = stack ? raw.map((_, k) => raw[0].map((_, i) => raw.slice(0, k + 1).reduce((a, r, j) => a + (hidden.has(j) ? 0 : r[i] ?? 0), 0))) : raw;
    const order = stack ? lines.map((_, k) => lines.length - 1 - k) : lines.map((_, k) => k);
    const data: uPlot.AlignedData = [xs.map(x => x / 1000), ...order.map(k => ys[k])];
    const span = xs.length > 1 ? xs[xs.length - 1] - xs[0] : 0;
    const xfmt = (v: number) => (span <= 15 * 60_000 ? fmtTime(v * 1000, true) : span <= 2 * 86_400_000 ? fmtTime(v * 1000) : new Date(v * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" }));
    const ink = css("--ink-3"), grid = css("--line"), font = `11px ${css("--font").split(",")[0]}`;
    const axis = (values?: uPlot.Axis["values"]): uPlot.Axis => ({ stroke: ink, grid: { stroke: grid, width: 1 }, ticks: { show: false }, font, values });
    const opts: uPlot.Options = {
      width: el.clientWidth, height: Math.max(80, el.clientHeight),
      legend: { show: false },
      cursor: { sync: { key: sync }, points: { size: 7 }, drag: { x: false, y: false } },
      scales: { x: { time: true }, y: { range: (_u, min, max) => [Math.min(0, min), max > 0 ? max * 1.08 : 1] } },
      axes: [{ ...axis((_u, splits) => splits.map(xfmt)), space: 70 }, { ...axis((_u, splits) => splits.map(v => fmtVal(v, unit))), size: yAxisWidth }],
      series: [{}, ...order.map(i => lines[i]).map((l, n) => ({
        label: l.label, stroke: lineColor(l), width: bars ? 0 : 2, show: !hidden.has(order[n]),
        fill: bars ? lineColor(l) : lines.length === 1 ? lineColor(l) + "22" : undefined,
        paths: bars ? uPlot.paths.bars!({ size: [0.7, 40], gap: 1 }) : undefined, points: { show: false }, spanGaps: false,
      }))],
      hooks: {
        draw: threshold === undefined ? [] : [u => {
          const y = u.valToPos(threshold, "y", true), ctx = u.ctx;
          ctx.save(); ctx.strokeStyle = css("--critical"); ctx.setLineDash([4, 4]); ctx.beginPath();
          ctx.moveTo(u.bbox.left, y); ctx.lineTo(u.bbox.left + u.bbox.width, y); ctx.stroke(); ctx.restore();
        }],
        setCursor: [u => {
          const t = tip.current, i = u.cursor.idx;
          if (!t) return;
          if (i == null || u.cursor.left == null || u.cursor.left < 0) { t.hidden = true; return; }
          const rows = lines.map((l, k) => ({ l, k, v: raw[k][i] })).filter(r => !hidden.has(r.k) && r.v != null).sort((a, b) => (b.v ?? 0) - (a.v ?? 0)).slice(0, 10);
          t.innerHTML = `<div class="t">${fmtDateTime((u.data[0][i] as number) * 1000)}</div>` + rows.map(r =>
            `<div class="row"><span><i style="background:${lineColor(r.l)}"></i>${escapeHtml(r.l.label)}</span><b class="num">${escapeHtml(fmtVal(r.v, unit))}</b></div>`).join("");
          t.hidden = rows.length === 0;
          const left = u.cursor.left + u.bbox.left / devicePixelRatio + 14;
          t.style.left = `${left + t.offsetWidth > el.clientWidth ? left - t.offsetWidth - 28 : left}px`;
          t.style.top = "8px";
        }],
      },
    };
    plot.current?.destroy();
    plot.current = new uPlot(opts, data, el);
    const ro = new ResizeObserver(() => plot.current?.setSize({ width: el.clientWidth, height: Math.max(80, el.clientHeight) }));
    ro.observe(el);
    return () => { ro.disconnect(); plot.current?.destroy(); plot.current = null; };
  }, [lines, unit, bars, theme, threshold, hidden]);

  return (
    <div class="tchart">
      <div class="chart" ref={box}><div class="tip" ref={tip} hidden /></div>
      {lines.length > 1 && (
        <div class="legend">
          {lines.map((l, i) => (
            <button type="button" class={hidden.has(i) ? "off" : ""} aria-pressed={!hidden.has(i)} onClick={() => setHidden(h => { const n = new Set(h); n.has(i) ? n.delete(i) : n.add(i); return n; })}>
              <i style={{ background: lineColor(l) }} />{l.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** Width of the y axis: the widest tick label plus padding, so units never clip. */
function yAxisWidth(u: uPlot, values: string[] | null): number {
  if (!values?.length) return 40;
  u.ctx.save();
  u.ctx.font = `${11 * devicePixelRatio}px ${css("--font").split(",")[0]}`;
  const w = Math.max(...values.map(v => u.ctx.measureText(v).width)) / devicePixelRatio;
  u.ctx.restore();
  return Math.ceil(w) + 16;
}

export function escapeHtml(s: string) { return s.replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!); }

export function Sparkline({ values, color = 0 }: { values: number[]; color?: number }) {
  if (values.length < 2) return null;
  const w = 200, h = 36, max = Math.max(...values, 1e-9), min = Math.min(...values, 0);
  const x = (i: number) => (i / (values.length - 1)) * (w - 4) + 2, y = (v: number) => h - 3 - ((v - min) / (max - min || 1)) * (h - 8);
  const d = values.map((v, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join("");
  const c = `var(--s${(color % 8) + 1})`;
  return (
    <svg class="spark" viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" aria-hidden="true">
      <path d={`${d}L${x(values.length - 1)},${h}L${x(0)},${h}Z`} fill={c} opacity="0.12" />
      <path d={d} fill="none" stroke={c} stroke-width="1.6" vector-effect="non-scaling-stroke" />
    </svg>
  );
}
