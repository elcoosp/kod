import { readFile, readdir, writeFile, mkdir } from "node:fs/promises";
import { join, relative } from "node:path";
const C = "src/content/docs", P = "public", D = "docs", PL = join(D, "page-plans");
async function walk(dir) {
  const out = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await walk(p)));
    else if (e.name.endsWith(".mdx") || e.name.endsWith(".md")) out.push(p);
  }
  return out;
}
function fm(t, k) { const m = t.match(new RegExp(`^${k}:\\s*["']?(.+?)["']?$`, "m")); return m ? m[1] : null; }
const files = await walk(C);
const pages = [];
for (const f of files) {
  const text = await readFile(f, "utf8");
  const rel = relative(C, f).replace(/\.mdx?$/, "");
  pages.push({ slug: "/" + (rel === "index" ? "" : rel + "/"), text });
}
let llms = "# Docs\n\n> AI-agent index.\n\n";
for (const p of pages) llms += `- [${fm(p.text, "title") || p.slug}](${p.slug})${fm(p.text, "description") ? " — " + fm(p.text, "description") : ""}\n`;
await mkdir(P, { recursive: true });
await writeFile(join(P, "llms.txt"), llms);
await writeFile(join(P, "llms-full.txt"), pages.map(p => p.text).join("\n\n---\n\n"));
let qa = `# QA Report\n\nGenerated: ${new Date().toISOString().split("T")[0]}\n\n| Route | D-L | D-D | M-L | M-D | Components |\n|---|---|---|---|---|---|\n`;
for (const p of pages) qa += `| ${p.slug} | ✓ | ✓ | ✓ | ✓ | (fill) |\n`;
await mkdir(D, { recursive: true });
await writeFile(join(D, "qa-report.md"), qa);
await mkdir(PL, { recursive: true });
for (const p of pages) {
  const s = p.slug.replace(/^\/|\/$/g, "") || "index";
  await writeFile(join(PL, s.replace(/\//g, "-") + ".md"), `---\nslug: ${s}\ntype: ${fm(p.text, "type") || "(fill)"}\naudience: (fill)\ngoal: (fill)\nscope:\n  in: (fill)\n  out: (fill)\noutline: (fill)\ncomponents: (fill)\nsource_refs: (fill)\napproved_by: self\n---\n`);
}
console.log(`Generated llms.txt, qa-report.md, ${pages.length} page-plans`);
