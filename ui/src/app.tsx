// App shell: navigation rail, shared time range and refresh, hash routes, token prompt.
import { useEffect, useState } from "preact/hooks";
import { api, authRequired, setToken } from "./api";
import { DashboardPage } from "./board";
import { ExplorePage } from "./explore";
import { LogsPage, TracesPage } from "./signals";
import { RANGES, type Range } from "./tqs";
import { Seg, ToastHost, useAsync } from "./ui";

function useHash() {
  const [hash, setHash] = useState(location.hash);
  useEffect(() => { const on = () => setHash(location.hash); addEventListener("hashchange", on); return () => removeEventListener("hashchange", on); }, []);
  return hash;
}

const Icon = { dash: "M3 3h7v9H3zM14 3h7v5h-7zM14 12h7v9h-7zM3 16h7v5H3z", explore: "M4 19l5-6 4 3 7-9", traces: "M4 6h10M8 12h12M6 18h7", logs: "M5 5h14M5 10h14M5 15h9M5 20h6" };
const NavIcon = ({ d }: { d: string }) => <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><path d={d} /></svg>;

export function App() {
  const hash = useHash();
  const [range, setRange] = useState<Range>(() => { try { return (localStorage.getItem("tiny-eye-range") as Range) || "1h"; } catch { return "1h"; } });
  const [live, setLive] = useState(true);
  const [tick, setTick] = useState(0);
  const [needToken, setNeedToken] = useState(false);
  const dashboards = useAsync(() => api.dashboards(), []);
  const catalog = useAsync(() => api.catalog(), [tick === 0 ? 0 : Math.floor(tick / 10)]);
  useEffect(() => { try { localStorage.setItem("tiny-eye-range", range); } catch { /* per-viewer convenience only */ } }, [range]);
  useEffect(() => { if (!live) return; const t = setInterval(() => setTick(x => x + 1), 30_000); return () => clearInterval(t); }, [live]);
  useEffect(() => { const on = () => setNeedToken(true); authRequired.addEventListener("required", on); return () => authRequired.removeEventListener("required", on); }, []);

  const list = dashboards.data ?? [];
  const [, page, arg] = (hash.replace(/^#\/?/, "/").split("/") as string[]);
  const route = page || "d";
  const dashId = route === "d" ? arg || list[0]?.id : undefined;

  return (
    <div class="app">
      <aside class="rail">
        <div class="brand">
          <svg width="24" height="24" viewBox="0 0 24 24" aria-hidden="true"><path d="M2 12c2.6-4.4 6.1-6.6 10-6.6s7.4 2.2 10 6.6c-2.6 4.4-6.1 6.6-10 6.6S4.6 16.4 2 12z" fill="none" stroke="currentColor" stroke-width="1.8" /><circle cx="12" cy="12" r="3.2" fill="var(--accent)" /></svg>
          tiny-eye
        </div>
        <nav class="nav" aria-label="Main">
          <a href={`#/d/${list[0]?.id ?? ""}`} aria-current={route === "d" ? "page" : undefined}><NavIcon d={Icon.dash} /><span>Dashboards</span></a>
          <a href="#/explore" aria-current={route === "explore" ? "page" : undefined}><NavIcon d={Icon.explore} /><span>Explore</span></a>
          <a href="#/traces" aria-current={route === "traces" ? "page" : undefined}><NavIcon d={Icon.traces} /><span>Traces</span></a>
          <a href="#/logs" aria-current={route === "logs" ? "page" : undefined}><NavIcon d={Icon.logs} /><span>Logs</span></a>
        </nav>
        <div>
          <div class="rail-label">Dashboards</div>
          <div class="dash-list">{list.map(d => <a class={d.id === dashId ? "on" : ""} href={`#/d/${d.id}`}>{d.title}</a>)}</div>
          {dashboards.error ? <div class="hint" style={{ padding: "0 10px" }}>Could not load dashboards.</div> : null}
        </div>
        <div class="rail-foot">
          <div class="rail-label" style={{ padding: 0 }}>Time range</div>
          <Seg label="Time range" value={range} onChange={setRange} options={RANGES.map(r => [r, r] as [Range, string])} />
          <button class="live" type="button" aria-pressed={live} onClick={() => setLive(l => !l)} style={{ marginTop: "8px" }}>
            <i style={live ? undefined : { background: "var(--ink-3)", boxShadow: "none" }} /><span>{live ? "Auto-refresh 30 s" : "Paused"}</span>
          </button>
        </div>
      </aside>
      <main>
        {route === "d" && (dashId
          ? <DashboardPage key={dashId} id={dashId} range={range} tick={tick} catalog={catalog.data} onSaved={dashboards.reload} />
          : <section class="view"><div class="empty">{dashboards.data ? "No dashboards yet. Add a YAML file to the dashboards directory (see Dashboards as code in the docs)." : "Loading…"}</div></section>)}
        {route === "explore" && <ExplorePage catalog={catalog.data} range={range} dashboards={list} onSaved={dashboards.reload} />}
        {route === "traces" && <TracesPage catalog={catalog.data} range={range} tick={tick} />}
        {route === "logs" && <LogsPage catalog={catalog.data} range={range} tick={tick} />}
      </main>
      {needToken && <TokenPrompt onDone={() => { setNeedToken(false); location.reload(); }} />}
      <ToastHost />
    </div>
  );
}

function TokenPrompt({ onDone }: { onDone: () => void }) {
  const [t, setT] = useState("");
  return (
    <>
      <div class="drawer-bg" />
      <div class="modal" role="dialog" aria-modal="true" aria-label="Access token"><form onSubmit={e => { e.preventDefault(); setToken(t.trim()); onDone(); }}>
        <h3>Access token needed</h3>
        <p class="hint">This tiny-eye node has auth enabled. Paste a bearer token with the <span class="mono">query</span> scope (<span class="mono">admin</span> to save dashboards). It is kept in this browser only.</p>
        <input class="input mono" type="password" value={t} onInput={e => setT((e.target as HTMLInputElement).value)} aria-label="Bearer token" autoFocus />
        <div class="np-actions"><button class="btn primary" type="submit" disabled={!t.trim()}>Use token</button></div>
      </form></div>
    </>
  );
}
