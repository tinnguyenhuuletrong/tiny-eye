// A small shop with a realistic shape: browsing is fast, search is slower, checkout calls the
// payments service, which is sometimes slow and sometimes fails. One file, two roles:
// SERVICE=shop-api (port 3000) or SERVICE=payments (port 3001).
import "./telemetry"; // first: registers the global tracer, meter and logger providers
import { context, logs, metrics, propagation, ROOT_CONTEXT, SeverityNumber, SpanKind, SpanStatusCode, trace } from "@opentelemetry/api";
import type { BunRequest } from "bun";

type Severity = "INFO" | "WARN" | "ERROR";
type Handler = (req: BunRequest) => Promise<Response>;

const SERVICE = process.env.SERVICE || "shop-api";
const PORT = Number(process.env.PORT || (SERVICE === "payments" ? 3001 : 3000));
const PAYMENTS_URL = process.env.PAYMENTS_URL || "http://127.0.0.1:3001";

const logger = logs.getLogger(SERVICE);
const meter = metrics.getMeter(SERVICE);
const tracer = trace.getTracer(SERVICE);
const orders = meter.createCounter("shop.orders", { description: "Orders placed" });
const cartItems = meter.createUpDownCounter("shop.cart.items", { description: "Items currently in carts" });
// Seconds need second-scale buckets: the SDK's default boundaries (0, 5, 10, 25 …) assume milliseconds.
const paymentDuration = meter.createHistogram("payments.authorize.duration", {
  unit: "s",
  description: "Card authorization time",
  advice: { explicitBucketBoundaries: [0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10] },
});
// What the Node HTTP instrumentation would emit; the dashboards read http_server_request_duration.
const requestDuration = meter.createHistogram("http.server.request.duration", {
  unit: "s",
  description: "Duration of HTTP server requests",
  advice: { explicitBucketBoundaries: [0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10] },
});

const sleep = (ms: number) => new Promise(r => setTimeout(r, ms));
const jitter = (base: number, spread: number) => base + Math.random() * spread;
const json = (body: unknown, status = 200) => Response.json(body, { status });
const body = (req: BunRequest): Promise<Record<string, unknown>> => req.json().catch(() => ({}));

function log(severity: Severity, text: string, attrs: Record<string, string> = {}) {
  const sev = { INFO: SeverityNumber.INFO, WARN: SeverityNumber.WARN, ERROR: SeverityNumber.ERROR }[severity];
  logger.emit({ severityNumber: sev, severityText: severity, body: text, attributes: attrs });
  if (severity !== "INFO") console.log(`[${SERVICE}] ${severity} ${text}`);
}

/** Server span + `http.server.request.duration` for one route, continuing the caller's trace. */
function traced(route: string, handler: Handler): Handler {
  return req => {
    const parent = propagation.extract(ROOT_CONTEXT, Object.fromEntries(req.headers));
    const attributes = { "http.request.method": req.method, "http.route": route, "url.path": new URL(req.url).pathname };
    return tracer.startActiveSpan(`${req.method} ${route}`, { kind: SpanKind.SERVER, attributes }, parent, async span => {
      const started = performance.now();
      let res: Response;
      try {
        res = await handler(req);
      } catch (e) {
        span.recordException(e as Error);
        res = json({ error: "internal" }, 500);
      }
      span.setAttribute("http.response.status_code", res.status);
      if (res.status >= 500) span.setStatus({ code: SpanStatusCode.ERROR, message: `HTTP ${res.status}` });
      span.end();
      requestDuration.record((performance.now() - started) / 1000, { ...attributes, "http.response.status_code": res.status });
      return res;
    });
  };
}

/** POST to the payments service as a client span, propagating the trace context. */
function authorize(order: number, method: string): Promise<{ ok: boolean; status: number }> {
  return tracer.startActiveSpan("POST /authorize", { kind: SpanKind.CLIENT }, async span => {
    const headers: Record<string, string> = { "content-type": "application/json" };
    propagation.inject(context.active(), headers);
    try {
      const r = await fetch(`${PAYMENTS_URL}/authorize`, { method: "POST", headers, body: JSON.stringify({ order, method }) });
      span.setAttribute("http.response.status_code", r.status);
      if (r.status >= 400) span.setStatus({ code: SpanStatusCode.ERROR });
      return { ok: r.ok, status: r.status };
    } catch {
      span.setStatus({ code: SpanStatusCode.ERROR, message: "connection failed" });
      return { ok: false, status: 503 };
    } finally {
      span.end();
    }
  });
}

const span = <T>(name: string, fn: () => Promise<T>, attrs: Record<string, string> = {}) =>
  tracer.startActiveSpan(name, { attributes: attrs }, async s => {
    try {
      return await fn();
    } finally {
      s.end();
    }
  });

// Card authorization: ~4 % declines from the provider, a slow tail of timeouts.
const authorizeCard: Handler = async req => {
  const started = performance.now();
  const slow = Math.random() < 0.06;
  await span("provider.charge", () => sleep(slow ? jitter(1500, 1200) : jitter(40, 120)));
  const seconds = (performance.now() - started) / 1000;
  const { order, method = "card" } = (await body(req)) as { order?: number; method?: string };
  if (slow && Math.random() < 0.5) {
    paymentDuration.record(seconds, { outcome: "timeout", payment_method: method });
    log("ERROR", `payment provider timeout after ${Math.round(seconds * 1000)}ms for order ${order}`, { order: String(order) });
    return json({ error: "provider_timeout" }, 504);
  }
  if (Math.random() < 0.04) {
    paymentDuration.record(seconds, { outcome: "declined", payment_method: method });
    log("WARN", `card declined for order ${order}: "insufficient_funds"`, { order: String(order) });
    return json({ error: "declined" }, 402);
  }
  paymentDuration.record(seconds, { outcome: "ok", payment_method: method });
  return json({ ok: true });
};

const checkout: Handler = async () => {
  const order = 10000 + Math.floor(Math.random() * 90000);
  const method = Math.random() < 0.8 ? "card" : "paypal";
  await span("SELECT cart_items", () => sleep(jitter(5, 15)));
  const r = await authorize(order, method);
  if (!r.ok) {
    trace.getActiveSpan()?.setStatus({ code: SpanStatusCode.ERROR, message: `payment failed: ${r.status}` });
    log("ERROR", `checkout failed for order ${order}: payment returned ${r.status}`, { order: String(order) });
    return json({ error: "payment_failed" }, 502);
  }
  await span("INSERT orders", () => sleep(jitter(6, 12)));
  orders.add(1, { payment_method: method });
  cartItems.add(-1);
  log("INFO", `order ${order} placed total ${(20 + Math.random() * 300).toFixed(2)}`, { order: String(order), payment_method: method });
  return json({ order });
};

const search: Handler = async req => {
  const q = new URL(req.url).searchParams.get("q") ?? "";
  const ms = jitter(30, 150) + (Math.random() < 0.05 ? 500 : 0);
  await span("SELECT products", () => sleep(ms), { "db.system": "postgresql" });
  if (ms > 400) log("WARN", `slow query "search_products" took ${Math.round(ms)}ms`, { query: q });
  return json({ q, hits: Math.floor(Math.random() * 50) });
};

const product: Handler = async req => {
  await sleep(jitter(4, 15));
  const { id } = req.params as { id: string };
  if (Math.random() < 0.02) {
    log("WARN", `product ${id} not found`);
    return json({ error: "not_found" }, 404);
  }
  return json({ id });
};

const routes: Record<string, { GET?: Handler; POST?: Handler }> =
  SERVICE === "payments"
    ? { "/authorize": { POST: traced("/authorize", authorizeCard) } }
    : {
        "/products": {
          GET: traced("/products", async () => {
            await sleep(jitter(5, 25));
            return json({ items: 24 });
          }),
        },
        "/products/:id": { GET: traced("/products/:id", product) },
        "/search": { GET: traced("/search", search) },
        "/cart": {
          POST: traced("/cart", async () => {
            await sleep(jitter(8, 20));
            cartItems.add(1);
            return json({ ok: true }, 201);
          }),
        },
        "/checkout": { POST: traced("/checkout", checkout) },
      };

Bun.serve({ port: PORT, routes, fetch: () => json({ error: "not_found" }, 404) });
console.log(`${SERVICE} listening on :${PORT}`);
