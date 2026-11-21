// A small shop with a realistic shape: browsing is fast, search is slower, checkout calls the
// payments service, which is sometimes slow and sometimes fails. One file, two roles:
// SERVICE=shop-api (port 3000) or SERVICE=payments (port 3001).
import express from "express";
import { context, logs, metrics, trace, SeverityNumber, SpanStatusCode } from "@opentelemetry/api";
import { getRPCMetadata } from "@opentelemetry/core";

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

const sleep = ms => new Promise(r => setTimeout(r, ms));
const jitter = (base, spread) => base + Math.random() * spread;
function log(severity, text, attrs = {}) {
  const sev = { INFO: SeverityNumber.INFO, WARN: SeverityNumber.WARN, ERROR: SeverityNumber.ERROR }[severity];
  logger.emit({ severityNumber: sev, severityText: severity, body: text, attributes: attrs });
  if (severity !== "INFO") console.log(`[${SERVICE}] ${severity} ${text}`);
}

const app = express();
app.use(express.json());
// Tag the request with its route template so the HTTP server metric carries `http.route`
// (the auto-instrumentation reads it from the request's RPC metadata when the response ends).
const route = path => (_req, _res, next) => {
  const md = getRPCMetadata(context.active());
  if (md) md.route = path;
  trace.getActiveSpan()?.setAttribute("http.route", path);
  next();
};

if (SERVICE === "payments") {
  // Card authorization: ~4 % declines from the provider, a slow tail of timeouts.
  app.post("/authorize", route("/authorize"), async (req, res) => {
    const started = performance.now();
    const slow = Math.random() < 0.06;
    await tracer.startActiveSpan("provider.charge", async span => {
      await sleep(slow ? jitter(1500, 1200) : jitter(40, 120));
      span.end();
    });
    const seconds = (performance.now() - started) / 1000;
    const method = req.body?.method || "card";
    if (slow && Math.random() < 0.5) {
      paymentDuration.record(seconds, { outcome: "timeout", payment_method: method });
      log("ERROR", `payment provider timeout after ${Math.round(seconds * 1000)}ms for order ${req.body?.order}`, { order: String(req.body?.order) });
      return res.status(504).json({ error: "provider_timeout" });
    }
    if (Math.random() < 0.04) {
      paymentDuration.record(seconds, { outcome: "declined", payment_method: method });
      log("WARN", `card declined for order ${req.body?.order}: "insufficient_funds"`, { order: String(req.body?.order) });
      return res.status(402).json({ error: "declined" });
    }
    paymentDuration.record(seconds, { outcome: "ok", payment_method: method });
    res.json({ ok: true });
  });
} else {
  app.get("/products", route("/products"), async (_req, res) => { await sleep(jitter(5, 25)); res.json({ items: 24 }); });
  app.get("/products/:id", route("/products/:id"), async (req, res) => {
    await sleep(jitter(4, 15));
    if (Math.random() < 0.02) { log("WARN", `product ${req.params.id} not found`); return res.status(404).json({ error: "not_found" }); }
    res.json({ id: req.params.id });
  });
  app.get("/search", route("/search"), async (req, res) => {
    const ms = jitter(30, 150) + (Math.random() < 0.05 ? 500 : 0);
    await tracer.startActiveSpan("SELECT products", async span => { span.setAttribute("db.system", "postgresql"); await sleep(ms); span.end(); });
    if (ms > 400) log("WARN", `slow query "search_products" took ${Math.round(ms)}ms`, { query: String(req.query.q || "") });
    res.json({ q: req.query.q, hits: Math.floor(Math.random() * 50) });
  });
  app.post("/cart", route("/cart"), async (_req, res) => { await sleep(jitter(8, 20)); cartItems.add(1); res.status(201).json({ ok: true }); });
  app.post("/checkout", route("/checkout"), async (req, res) => {
    const order = 10000 + Math.floor(Math.random() * 90000);
    const method = Math.random() < 0.8 ? "card" : "paypal";
    await tracer.startActiveSpan("SELECT cart_items", async span => { await sleep(jitter(5, 15)); span.end(); });
    const r = await fetch(`${PAYMENTS_URL}/authorize`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ order, method }) }).catch(e => ({ ok: false, status: 503, e }));
    if (!r.ok) {
      const span = trace.getActiveSpan();
      span?.setStatus({ code: SpanStatusCode.ERROR, message: `payment failed: ${r.status}` });
      log("ERROR", `checkout failed for order ${order}: payment returned ${r.status}`, { order: String(order) });
      return res.status(502).json({ error: "payment_failed" });
    }
    await tracer.startActiveSpan("INSERT orders", async span => { await sleep(jitter(6, 12)); span.end(); });
    orders.add(1, { payment_method: method });
    cartItems.add(-1);
    log("INFO", `order ${order} placed total ${(20 + Math.random() * 300).toFixed(2)}`, { order: String(order), payment_method: method });
    res.json({ order });
  });
}

app.listen(PORT, () => console.log(`${SERVICE} listening on :${PORT}`));
