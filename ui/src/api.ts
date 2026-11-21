// Typed client for the tiny-eye HTTP API. Every type comes from api.gen.ts, which is generated
// from api/openapi.yaml on each build — the UI cannot drift from the contract.
import type { components } from "./api.gen";

type S = components["schemas"];
export type Tqs = S["TqsDocument"];
export type QueryResponse = S["QueryResponse"];
export type Series = S["Series"];
export type Problem = S["Problem"];
export type Dashboard = S["Dashboard"];
export type Panel = S["Panel"];
export type PanelUi = S["PanelUi"];
export type DashboardSummary = S["DashboardSummary"];
export type Catalog = S["CatalogResponse"];
export type CatalogMetric = S["CatalogMetric"];

const TOKEN_KEY = "tiny-eye-token";
export function getToken(): string {
  try { return localStorage.getItem(TOKEN_KEY) || ""; } catch { return ""; }
}
export function setToken(t: string) {
  try { t ? localStorage.setItem(TOKEN_KEY, t) : localStorage.removeItem(TOKEN_KEY); } catch { /* storage blocked: token lasts for this tab */ }
}

export class ApiError extends Error {
  constructor(public status: number, public problems: Problem[], message: string) { super(message); }
}

/** Fires when the API answers 401, so the app can ask for a token. */
export const authRequired = new EventTarget();

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(path, { method, headers, body: body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body) });
  const text = await res.text();
  if (res.status === 401) authRequired.dispatchEvent(new Event("required"));
  if (!res.ok) {
    let problems: Problem[] = [];
    try { problems = (JSON.parse(text) as S["ProblemResponse"]).problems ?? []; } catch { /* plain-text error */ }
    throw new ApiError(res.status, problems, problems.length ? problems.map(p => `${p.code} at ${p.path}: ${p.message}`).join("; ") : text.trim() || res.statusText);
  }
  return (text ? JSON.parse(text) : undefined) as T;
}

export const api = {
  query: (doc: Tqs | string) => call<QueryResponse>("POST", "/api/v1/query", doc),
  validate: (doc: Tqs | string) => call<S["ProblemResponse"]>("POST", "/api/v1/validate", doc),
  catalog: () => call<Catalog>("GET", "/api/v1/catalog"),
  dashboards: () => call<DashboardSummary[]>("GET", "/api/v1/dashboards"),
  dashboard: (id: string) => call<Dashboard>("GET", `/api/v1/dashboards/${encodeURIComponent(id)}`),
  saveDashboard: (d: Dashboard) => call<Dashboard>("PUT", `/api/v1/dashboards/${encodeURIComponent(d.id)}`, d),
};
