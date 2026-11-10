import { chromium } from "playwright";
import { readdir, mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
const BASE = process.env.QA_BASE_URL ?? "http://localhost:4321";
const OUT = ".qa/screenshots";
async function walk(dir) {
  const entries = await readdir(dir, { withFileTypes: true });
  const files = [];
  for (const e of entries) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) files.push(...(await walk(p)));
    else if (e.name === "index.html") files.push(p);
  }
  return files;
}
const all = (await walk("dist")).map(f => "/" + path.relative("dist", path.dirname(f)).replace(/\.$/, ""));
const VP = { desktop: { width: 1440, height: 900 }, mobile: { width: 390, height: 844 } };
const fails = [];
const fail = (r, v, t, k, d) => fails.push({ route: r, viewport: v, theme: t, kind: k, detail: d });
const browser = await chromium.launch();
await mkdir(OUT, { recursive: true });
for (const route of all) for (const [vpn, vp] of Object.entries(VP)) for (const theme of ["light", "dark"]) {
  const ctx = await browser.newContext({ viewport: vp });
  const page = await ctx.newPage();
  page.on("console", m => { if (m.type() === "error") fail(route, vpn, theme, "console", m.text()); });
  page.on("pageerror", e => fail(route, vpn, theme, "pageerror", e.message));
  page.on("response", r => { if (r.status() >= 400) fail(route, vpn, theme, "http", String(r.status())); });
  await page.addInitScript(t => { try { localStorage.setItem("starlight-theme", t); } catch {} }, theme);
  await page.goto(BASE + route, { waitUntil: "networkidle" });
  const probe = await page.evaluate(() => ({ h1: document.querySelectorAll("h1").length, overflow: document.documentElement.scrollWidth > window.innerWidth + 1 }));
  if (probe.h1 !== 1) fail(route, vpn, theme, "h1", String(probe.h1));
  if (probe.overflow) fail(route, vpn, theme, "overflow", "");
  const slug = route === "/" ? "index" : route.replace(/^\/|\/$/g, "").replace(/\//g, "_");
  await page.screenshot({ path: path.join(OUT, `${slug}--${vpn}--${theme}.png`), fullPage: true });
  await ctx.close();
}
await browser.close();
await writeFile(".qa/failures.json", JSON.stringify(fails, null, 2));
if (fails.length) { console.error(`QA FAILED — ${fails.length}`); process.exit(1); }
console.log(`QA PASSED — ${all.length} routes`);
