#!/usr/bin/env node
// Records the docs media with headless Chrome (DevTools protocol) and ffmpeg. No npm deps.
//
//   node docs/assets/media/record.mjs how-it-works      animated diagram -> docs/assets/how-it-works.{gif,mp4,png}
//   node docs/assets/media/record.mjs getting-started   terminal scene + live UI tour -> docs/assets/getting-started.{gif,mp4,png}
//
// getting-started needs the demo running with traffic: make demo, make demo-app, make demo-load.
// Env: CHROME (browser binary), TINY_EYE_URL (default http://127.0.0.1:7070).
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(HERE, "..");
const URL_ = process.env.TINY_EYE_URL ?? "http://127.0.0.1:7070";
const CHROME =
  process.env.CHROME ??
  ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/usr/bin/google-chrome", "/usr/bin/chromium"].find(existsSync);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- minimal CDP client ----------
async function browser(width, height) {
  const profile = mkdtempSync(join(tmpdir(), "tiny-eye-rec-"));
  const port = 9300 + Math.floor(Math.random() * 500);
  const proc = spawn(CHROME, [
    "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`,
    "--hide-scrollbars", "--no-first-run", "--no-default-browser-check", "--force-color-profile=srgb",
    `--window-size=${width},${height}`, "about:blank",
  ], { stdio: "ignore" });
  let info;
  for (let i = 0; i < 100 && !info; i++) {
    await sleep(100);
    info = await fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json()).then((l) => l.find((t) => t.type === "page")).catch(() => null);
  }
  const ws = new WebSocket(info.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  let id = 0;
  const pending = new Map();
  const listeners = [];
  ws.addEventListener("message", (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { const { ok, err } = pending.get(m.id); pending.delete(m.id); m.error ? err(new Error(m.error.message)) : ok(m.result); }
    else if (m.method) listeners.forEach((f) => f(m));
  });
  const send = (method, params = {}) => new Promise((ok, err) => { const i = ++id; pending.set(i, { ok, err }); ws.send(JSON.stringify({ id: i, method, params })); });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(`${expression.slice(0, 80)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value;
  };
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
  const navigate = async (url) => {
    const loaded = new Promise((r) => { const f = (m) => m.method === "Page.loadEventFired" && r(); listeners.push(f); });
    await send("Page.navigate", { url });
    await loaded;
    await evaluate("document.fonts.ready.then(() => true)");
  };
  const shot = async () => Buffer.from((await send("Page.captureScreenshot", { format: "png" })).data, "base64");
  const close = () => { ws.close(); proc.kill(); setTimeout(() => rmSync(profile, { recursive: true, force: true }), 500); };
  return { send, evaluate, navigate, shot, close };
}

// ---------- encoding ----------
// frames: [{file, dur}] -> mp4 (h264) + gif (palette) + png poster
function encode(name, dir, frames, { gifWidth = 960, gifFps = 12, poster = 0 } = {}) {
  const list = frames.map((f) => `file '${f.file}'\nduration ${f.dur.toFixed(4)}`).join("\n") + `\nfile '${frames.at(-1).file}'\n`;
  writeFileSync(join(dir, "list.txt"), list);
  const ff = (...a) => execFileSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", ...a], { stdio: "inherit" });
  const mp4 = join(OUT, `${name}.mp4`), gif = join(OUT, `${name}.gif`), png = join(OUT, `${name}.png`);
  ff("-f", "concat", "-safe", "0", "-i", join(dir, "list.txt"), "-vf", "fps=30,format=yuv420p", "-c:v", "libx264", "-crf", "22", "-preset", "slow", "-movflags", "+faststart", "-an", mp4);
  ff("-i", mp4, "-vf", `fps=${gifFps},scale=${gifWidth}:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle`, "-loop", "0", gif);
  ff("-i", frames[poster].file, png);
  for (const f of [mp4, gif, png]) console.log(`wrote ${f} (${(execFileSync("stat", process.platform === "darwin" ? ["-f%z", f] : ["-c%s", f]).toString().trim() / 1e6).toFixed(2)} MB)`);
}

// Deterministic pages expose window.render(t) and window.LOOP_SECONDS: step them frame by frame.
async function recordAnimation(name, page, { width = 1200, height = 640, fps = 30, poster } = {}) {
  const b = await browser(width, height);
  const dir = mkdtempSync(join(tmpdir(), `tiny-eye-${name}-`));
  try {
    await b.navigate(pathToFileURL(join(HERE, page)).href + "?still");
    const seconds = await b.evaluate("window.LOOP_SECONDS");
    const frames = [];
    for (let i = 0; i < seconds * fps; i++) {
      await b.evaluate(`render(${i / fps})`);
      const file = join(dir, `f${String(i).padStart(5, "0")}.png`);
      writeFileSync(file, await b.shot());
      frames.push({ file, dur: 1 / fps });
    }
    encode(name, dir, frames, { poster: poster ?? Math.round(frames.length * 0.78) });
  } finally { b.close(); rmSync(dir, { recursive: true, force: true }); }
}

// ---------- live UI tour ----------
// A fake cursor glides to each target before it is clicked, so the video reads like a person using it.
const CURSOR = `(() => {
  if (document.getElementById('rec-cursor')) return;
  const c = document.createElement('div'); c.id = 'rec-cursor';
  c.innerHTML = '<svg width="22" height="22" viewBox="0 0 24 24"><path d="M4 2l16 9-7 2-3 7z" fill="#fff" stroke="#111" stroke-width="1.5" stroke-linejoin="round"/></svg>';
  Object.assign(c.style, { position: 'fixed', left: '640px', top: '420px', zIndex: 99999, pointerEvents: 'none', transition: 'left .7s cubic-bezier(.4,0,.2,1), top .7s cubic-bezier(.4,0,.2,1)', filter: 'drop-shadow(0 2px 3px rgba(0,0,0,.4))' });
  document.body.appendChild(c);
  window.__ripple = (x, y) => { const r = document.createElement('div'); Object.assign(r.style, { position: 'fixed', left: x - 14 + 'px', top: y - 14 + 'px', width: '28px', height: '28px', borderRadius: '50%', background: 'rgba(94,234,212,.45)', zIndex: 99998, pointerEvents: 'none', transition: 'transform .45s, opacity .45s' }); document.body.appendChild(r); requestAnimationFrame(() => { r.style.transform = 'scale(2)'; r.style.opacity = '0'; }); setTimeout(() => r.remove(), 500); };
})()`;

async function recordTour(name, steps, { width = 1440, height = 900, fps = 12 } = {}) {
  const b = await browser(width, height);
  const dir = mkdtempSync(join(tmpdir(), `tiny-eye-${name}-`));
  const frames = [];
  let recording = true, n = 0, loop = null;
  // Capture starts after the first page load (a screenshot requested mid-navigation can stall).
  const capture = async () => {
    let last = performance.now();
    while (recording) {
      const png = await Promise.race([b.shot(), sleep(3000).then(() => null)]);
      if (!png) continue;
      const now = performance.now();
      if (frames.length) frames.at(-1).dur = (now - last) / 1000;
      const file = join(dir, `f${String(n++).padStart(5, "0")}.png`);
      writeFileSync(file, png);
      frames.push({ file, dur: 1 / fps });
      last = now;
      const wait = 1000 / fps - (performance.now() - now);
      if (wait > 0) await sleep(wait);
    }
  };
  const at = async (selector) => {
    const r = await b.evaluate(`(() => { const e = ${selector}; if (!e) return null; e.scrollIntoView({block:'nearest'}); const r = e.getBoundingClientRect(); return {x: r.left + r.width/2, y: r.top + r.height/2}; })()`);
    if (!r) throw new Error(`not found: ${selector}`);
    return r;
  };
  const api = {
    b,
    sleep,
    async go(path) { await b.navigate(URL_ + path); await b.evaluate(CURSOR); loop ??= capture(); },
    async move(selector) { const p = await at(selector); await b.evaluate(`Object.assign(document.getElementById('rec-cursor').style, {left: '${p.x}px', top: '${p.y}px'})`); await sleep(800); await b.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: p.x, y: p.y }); return p; },
    async hover(x, y) { await b.evaluate(`Object.assign(document.getElementById('rec-cursor').style, {left: '${x}px', top: '${y}px'})`); await b.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y }); },
    async click(selector) {
      const p = await api.move(selector);
      await b.evaluate(`__ripple(${p.x}, ${p.y})`);
      await b.send("Input.dispatchMouseEvent", { type: "mousePressed", x: p.x, y: p.y, button: "left", clickCount: 1 });
      await b.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: p.x, y: p.y, button: "left", clickCount: 1 });
      await sleep(300);
    },
    async drag(selector, dx, dy) {
      const p = await api.move(selector);
      await b.send("Input.dispatchMouseEvent", { type: "mousePressed", x: p.x, y: p.y, button: "left", buttons: 1, clickCount: 1 });
      for (let i = 1; i <= 20; i++) {
        const x = p.x + (dx * i) / 20, y = p.y + (dy * i) / 20;
        await b.evaluate(`Object.assign(document.getElementById('rec-cursor').style, {transition: 'none', left: '${x}px', top: '${y}px'})`);
        await b.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, button: "left", buttons: 1 });
        await sleep(40);
      }
      await b.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: p.x + dx, y: p.y + dy, button: "left", buttons: 0, clickCount: 1 });
      await b.evaluate(`document.getElementById('rec-cursor').style.transition = 'left .7s cubic-bezier(.4,0,.2,1), top .7s cubic-bezier(.4,0,.2,1)'`);
    },
    async type(text, delay = 45) { for (const ch of text) { await b.send("Input.insertText", { text: ch }); await sleep(delay); } },
    async waitFor(expr, ms = 15000) { const end = Date.now() + ms; while (Date.now() < end) { if (await b.evaluate(`!!(${expr})`)) return; await sleep(150); } throw new Error(`timeout: ${expr}`); },
  };
  try {
    await steps(api);
  } finally {
    recording = false;
    await loop;
    b.close();
  }
  encode(name, dir, frames, { gifWidth: 960, gifFps: 10, poster: Math.min(frames.length - 1, Math.round(frames.length * 0.15)) });
  rmSync(dir, { recursive: true, force: true });
}

// Element lookup by visible text, for steps below.
const byText = (sel, text) => `[...document.querySelectorAll(${JSON.stringify(sel)})].find(e => e.textContent.trim().startsWith(${JSON.stringify(text)}))`;

const JOBS = {
  "how-it-works": () => recordAnimation("how-it-works", "how-it-works.html"),
  "getting-started": async () => {
    // 1. terminal scene (deterministic page), 2. live UI tour, then joined.
    if (!existsSync(join(OUT, "gs-terminal.mp4"))) await recordAnimation("gs-terminal", "getting-started-terminal.html", { width: 1440, height: 900, fps: 24, poster: 0 });
    await recordTour("gs-ui", async (t) => {
      await t.go("/#/d/shop");
      await t.waitFor(`document.querySelectorAll('.u-over').length > 3`);
      await t.sleep(1800);
      // sweep the shared crosshair across a chart
      const r = await t.b.evaluate(`(() => { const e = document.querySelectorAll('.u-over')[0].getBoundingClientRect(); return {x: e.left, y: e.top + e.height/2, w: e.width}; })()`);
      for (let i = 0; i <= 24; i++) { await t.hover(r.x + (r.w * i) / 24, r.y); await t.sleep(70); }
      await t.sleep(600);
      await t.click(byText("button", "Edit layout"));
      await t.sleep(900);
      // resize the first chart narrower, look, then Cancel (the tour never writes the dashboard file)
      await t.drag(`document.querySelectorAll('.resize')[4]`, -260, 0);
      await t.sleep(1400);
      await t.click(byText("button", "Cancel"));
      await t.sleep(600);
      await t.click(`document.querySelector('a[href="#/explore"]')`);
      await t.sleep(2200);
      await t.click(byText("button", "TQS"));
      await t.sleep(2200);
      await t.click(byText("button", "Builder"));
      await t.sleep(1200);
      await t.click(`document.querySelector('a[href="#/traces"]')`);
      await t.waitFor(`document.querySelector('tr.click')`);
      await t.sleep(1500);
      await t.click(`document.querySelector('tr.click')`);
      await t.sleep(3000);
      await t.b.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
      await t.sleep(400);
      await t.click(`document.querySelector('a[href="#/logs"]')`);
      await t.sleep(2200);
      await t.click(byText("button", "Patterns"));
      await t.sleep(2600);
    });
    const dir = mkdtempSync(join(tmpdir(), "tiny-eye-gs-"));
    const a = join(OUT, "gs-terminal.mp4"), u = join(OUT, "gs-ui.mp4");
    writeFileSync(join(dir, "l.txt"), `file '${a}'\nfile '${u}'\n`);
    execFileSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i", join(dir, "l.txt"), "-c", "copy", join(dir, "all.mp4")]);
    // re-encode via encode() for consistent gif/png
    execFileSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", "-i", join(dir, "all.mp4"), "-vf", "fps=8", join(dir, "f%05d.png")]);
    const files = execFileSync("ls", [dir]).toString().split("\n").filter((f) => f.startsWith("f")).map((f) => ({ file: join(dir, f), dur: 1 / 8 }));
    execFileSync("cp", [join(dir, "all.mp4"), join(OUT, "getting-started.mp4")]);
    execFileSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", "-i", join(dir, "all.mp4"), "-vf", "fps=8,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle", "-loop", "0", join(OUT, "getting-started.gif")]);
    execFileSync("cp", [files[Math.round(files.length * 0.55)].file, join(OUT, "getting-started.png")]);
    for (const f of ["gs-terminal", "gs-ui"]) for (const ext of ["mp4", "gif", "png"]) rmSync(join(OUT, `${f}.${ext}`), { force: true });
    rmSync(dir, { recursive: true, force: true });
    console.log("wrote docs/assets/getting-started.{mp4,gif,png}");
  },
};

const job = JOBS[process.argv[2]];
if (!job) { console.error(`usage: record.mjs <${Object.keys(JOBS).join("|")}>`); process.exit(2); }
await job();
