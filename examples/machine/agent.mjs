#!/usr/bin/env node
// Machine agent: sends this computer's CPU, memory, load, network, disk and busiest processes to
// tiny-eye as OTLP/HTTP JSON every few seconds, plus a WARN log when a process pins a CPU core.
// No dependencies (Node 20+). macOS and Linux.
//
//   node examples/machine/agent.mjs                  (or: make machine-agent)
//
// Env: OTEL_EXPORTER_OTLP_ENDPOINT (default http://127.0.0.1:7070), MACHINE_NAME (default: hostname),
//      MACHINE_INTERVAL seconds (default 5), MACHINE_TOP processes to report (default 8),
//      MACHINE_PROCESSES=user to report only processes of the user running the agent (default: all).
import { execFileSync } from "node:child_process";
import os from "node:os";
import { existsSync, readFileSync } from "node:fs";

const ENDPOINT = (process.env.OTEL_EXPORTER_OTLP_ENDPOINT ?? "http://127.0.0.1:7070").replace(/\/$/, "");
const NAME = process.env.MACHINE_NAME ?? os.hostname().replace(/\.local$/, "");
const INTERVAL = Number(process.env.MACHINE_INTERVAL ?? 5) * 1000;
const TOP = Number(process.env.MACHINE_TOP ?? 8);
const ONLY_MINE = process.env.MACHINE_PROCESSES === "user";
const START_NS = `${BigInt(Date.now()) * 1_000_000n}`;
// Every command gets a timeout, so one slow tool never stalls the samples.
const run = (cmd, args) => execFileSync(cmd, args, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], timeout: 3000 });
const tryRead = (f, fallback) => { try { return f(); } catch { return fallback; } };

// ---------- readings ----------
let lastCpu = null;
function cpuUtilization() {
  const sum = { user: 0, system: 0, idle: 0, other: 0 };
  for (const { times: t } of os.cpus()) { sum.user += t.user + t.nice; sum.system += t.sys; sum.idle += t.idle; sum.other += t.irq; }
  const prev = lastCpu;
  lastCpu = sum;
  if (!prev) return null;
  const d = Object.fromEntries(Object.keys(sum).map((k) => [k, sum[k] - prev[k]]));
  const total = d.user + d.system + d.idle + d.other || 1;
  return { user: d.user / total, system: (d.system + d.other) / total, idle: d.idle / total };
}

function memory() {
  const total = os.totalmem();
  if (process.platform === "linux") {
    const kb = (k) => Number(/^\S+:\s+(\d+)/m.exec(readFileSync("/proc/meminfo", "utf8").split("\n").find((l) => l.startsWith(k + ":")) ?? "")?.[1] ?? 0) * 1024;
    const available = kb("MemAvailable");
    return { total, used: total - available, available };
  }
  // macOS: free memory is small by design; inactive and speculative pages are available too.
  const vm = run("vm_stat", []);
  const page = Number(/page size of (\d+)/.exec(vm)?.[1] ?? 4096);
  const pages = (label) => Number(new RegExp(`${label}:\\s+(\\d+)`).exec(vm)?.[1] ?? 0);
  const available = (pages("Pages free") + pages("Pages inactive") + pages("Pages speculative")) * page;
  return { total, used: total - available, available };
}

function network() {
  const out = [];
  if (process.platform === "linux") {
    for (const line of readFileSync("/proc/net/dev", "utf8").split("\n").slice(2)) {
      const m = /^\s*([^:]+):\s*(\d+)(?:\s+\d+){7}\s+(\d+)/.exec(line);
      if (m && m[1] !== "lo") out.push({ device: m[1], rx: Number(m[2]), tx: Number(m[3]) });
    }
    return out;
  }
  // netstat -ibn: one <Link#n> row per interface carries the byte counters (-n: no DNS lookups).
  for (const line of run("netstat", ["-ibn"]).split("\n").slice(1)) {
    const c = line.trim().split(/\s+/);
    if (c.length >= 10 && c[2]?.startsWith("<Link#") && c[0] !== "lo0") {
      const rx = Number(c.at(-5)), tx = Number(c.at(-2));
      if (rx > 0 || tx > 0) out.push({ device: c[0], rx, tx });
    }
  }
  return out;
}

// macOS keeps user data on /System/Volumes/Data; "/" is the small read-only system volume.
const MOUNT = process.platform === "darwin" && existsSync("/System/Volumes/Data") ? "/System/Volumes/Data" : "/";
function disk() {
  const c = run("df", ["-k", MOUNT]).trim().split("\n").at(-1).split(/\s+/);
  const used = Number(c[2]) * 1024, free = Number(c[3]) * 1024;
  return { used, free };
}

function processes() {
  // pcpu is percent of one core, so a process spinning on one core reads 100.
  const who = ONLY_MINE ? ["-U", String(os.userInfo().uid)] : ["-A"];
  return run("ps", [...who, "-o", "pid=,pcpu=,rss=,comm="]).trim().split("\n").map((l) => {
    const m = /^\s*(\d+)\s+([\d.]+)\s+(\d+)\s+(.*)$/.exec(l);
    return m && { pid: m[1], cpu: Number(m[2]) / 100, rss: Number(m[3]) * 1024, name: m[4].split("/").at(-1).slice(0, 40) };
  }).filter((p) => p && p.pid !== String(process.pid)).sort((a, b) => b.cpu - a.cpu).slice(0, TOP);
}

// ---------- OTLP JSON ----------
const attrs = (o) => Object.entries(o).map(([key, v]) => ({ key, value: { stringValue: String(v) } }));
const resource = { attributes: attrs({ "service.name": "machine", "service.instance.id": NAME, "host.name": NAME, "os.type": process.platform }) };
const gauge = (name, unit, points) => ({ name, unit, gauge: { dataPoints: points } });
const counter = (name, unit, points) => ({ name, unit, sum: { aggregationTemporality: 2, isMonotonic: true, dataPoints: points } });

async function post(path, body) {
  const res = await fetch(`${ENDPOINT}${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status} ${await res.text()}`);
}

let reported = new Map(); // processes sent last tick: "name\0pid" -> labels
const hot = new Map(); // pid -> consecutive intervals at >= 80 % of a core
const warned = new Map(); // pid -> last warning time
async function tick() {
  const now = `${BigInt(Date.now()) * 1_000_000n}`;
  const pt = (asDouble, a = {}) => ({ asDouble, timeUnixNano: now, attributes: attrs(a) });
  const sumPt = (asDouble, a = {}) => ({ ...pt(asDouble, a), startTimeUnixNano: START_NS });
  const metrics = [];
  const cpu = cpuUtilization();
  if (cpu) metrics.push(gauge("system.cpu.utilization", "1", Object.entries(cpu).map(([state, v]) => pt(v, { state }))));
  const [l1, l5, l15] = os.loadavg();
  metrics.push(gauge("system.cpu.load_average.1m", "1", [pt(l1)]), gauge("system.cpu.load_average.5m", "1", [pt(l5)]), gauge("system.cpu.load_average.15m", "1", [pt(l15)]));
  metrics.push(gauge("system.cpu.logical.count", "1", [pt(os.cpus().length)]));
  const mem = tryRead(memory, { total: os.totalmem(), used: os.totalmem() - os.freemem(), available: os.freemem() });
  metrics.push(gauge("system.memory.usage", "By", [pt(mem.used, { state: "used" }), pt(mem.available, { state: "available" })]));
  metrics.push(gauge("system.memory.utilization", "1", [pt(mem.used / mem.total)]));
  const net = tryRead(network, []);
  if (net.length) metrics.push(counter("system.network.io", "By", net.flatMap((n) => [sumPt(n.rx, { device: n.device, direction: "receive" }), sumPt(n.tx, { device: n.device, direction: "transmit" })])));
  const fs = tryRead(disk, null);
  if (fs) metrics.push(gauge("system.filesystem.usage", "By", [pt(fs.used, { mountpoint: MOUNT, state: "used" }), pt(fs.free, { mountpoint: MOUNT, state: "free" })]));
  if (fs) metrics.push(gauge("system.filesystem.utilization", "1", [pt(fs.used / (fs.used + fs.free), { mountpoint: MOUNT })]));
  const procs = tryRead(processes, []);
  // A process that left the top list (or exited) gets a staleness marker (OTLP "no recorded value"),
  // so tiny-eye stops showing its last value instead of carrying it forward for 5 minutes.
  const now_keys = new Map(procs.map((p) => [`${p.name}\u0000${p.pid}`, { process_name: p.name, pid: p.pid }]));
  const gone = [...reported].filter(([k]) => !now_keys.has(k)).map(([, a]) => ({ timeUnixNano: now, flags: 1, attributes: attrs(a) }));
  reported = now_keys;
  metrics.push(gauge("process.cpu.utilization", "1", [...procs.map((p) => pt(p.cpu, { process_name: p.name, pid: p.pid })), ...gone]));
  metrics.push(gauge("process.memory.usage", "By", [...procs.map((p) => pt(p.rss, { process_name: p.name, pid: p.pid })), ...gone]));
  await post("/v1/metrics", { resourceMetrics: [{ resource, scopeMetrics: [{ scope: { name: "tiny-eye-machine-agent" }, metrics }] }] });

  // A WARN log when a process holds >= 80 % of a core for three intervals in a row (at most every 5 min).
  const logs = [];
  for (const p of procs) {
    const n = p.cpu >= 0.8 ? (hot.get(p.pid) ?? 0) + 1 : 0;
    hot.set(p.pid, n);
    if (n >= 3 && Date.now() - (warned.get(p.pid) ?? 0) > 300_000) {
      warned.set(p.pid, Date.now());
      logs.push({ timeUnixNano: now, severityNumber: 13, severityText: "WARN", body: { stringValue: `process ${p.name} (pid ${p.pid}) has used ${Math.round(p.cpu * 100)}% of a CPU core for ${Math.round((n * INTERVAL) / 1000)}s` }, attributes: attrs({ process_name: p.name, pid: p.pid }) });
    }
  }
  if (logs.length) await post("/v1/logs", { resourceLogs: [{ resource, scopeLogs: [{ scope: { name: "tiny-eye-machine-agent" }, logRecords: logs }] }] });
}

async function hello() {
  const gb = (os.totalmem() / 2 ** 30).toFixed(0);
  const body = `machine agent started on ${NAME}: ${os.cpus().length} CPU cores, ${gb} GB RAM, every ${INTERVAL / 1000}s`;
  await post("/v1/logs", { resourceLogs: [{ resource, scopeLogs: [{ scope: { name: "tiny-eye-machine-agent" }, logRecords: [{ timeUnixNano: `${BigInt(Date.now()) * 1_000_000n}`, severityNumber: 9, severityText: "INFO", body: { stringValue: body } }] }] }] });
  console.log(`${body} -> ${ENDPOINT}`);
}

let failing = false;
const loop = async () => {
  try { await tick(); if (failing) console.log("tiny-eye reachable again"); failing = false; }
  catch (e) { if (!failing) console.error(`cannot send to ${ENDPOINT}: ${e.message} (retrying)`); failing = true; }
};
await hello().catch((e) => console.error(`cannot reach ${ENDPOINT}: ${e.message} (will keep trying)`));
cpuUtilization();
setInterval(loop, INTERVAL);
setTimeout(loop, 1000);
