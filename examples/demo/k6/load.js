// k6 load for the demo shop: mostly browsing, some carts, a few checkouts.
// docker run --rm -i -e BASE_URL=http://host.docker.internal:3000 grafana/k6 run - < k6/load.js
import http from "k6/http";
import { check, sleep } from "k6";

const BASE = __ENV.BASE_URL || "http://127.0.0.1:3000";
const DURATION = __ENV.DURATION || "3m";

export const options = {
  scenarios: {
    browse: { executor: "constant-arrival-rate", rate: 40, timeUnit: "1s", duration: DURATION, preAllocatedVUs: 20, exec: "browse" },
    shop: { executor: "ramping-arrival-rate", startRate: 2, timeUnit: "1s", preAllocatedVUs: 20, exec: "shop",
            stages: [{ target: 8, duration: "1m" }, { target: 8, duration: DURATION }] },
  },
  thresholds: { http_req_failed: ["rate<0.15"] },
};

export function browse() {
  const r = Math.random();
  if (r < 0.5) http.get(`${BASE}/products`);
  else if (r < 0.8) http.get(`${BASE}/products/${Math.floor(Math.random() * 500)}`);
  else http.get(`${BASE}/search?q=${["shoes", "lamp", "desk", "mug"][Math.floor(Math.random() * 4)]}`);
}

export function shop() {
  http.post(`${BASE}/cart`, JSON.stringify({ sku: "A-1" }), { headers: { "Content-Type": "application/json" } });
  const res = http.post(`${BASE}/checkout`, "{}", { headers: { "Content-Type": "application/json" } });
  check(res, { "checkout ok or expected failure": r => [200, 502].includes(r.status) });
  sleep(0.2);
}
