import { jsx, jsxs } from "react/jsx-runtime";
import React from "react";
const VERSION = "v12-refined";
function hexToRgba(hex, a) {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}
function getSection(pathname) {
  const p = "/" + (pathname || "").toLowerCase().replace(/^\/+/, "");
  if (p.includes("404")) return "404";
  if (p.startsWith("/tutorials/") || p.startsWith("/getting-started/")) return "Tutorial";
  if (p.startsWith("/guides/")) return "Guide";
  if (p.startsWith("/reference/")) return "Reference";
  if (p.startsWith("/explanation/") || p.startsWith("/concepts/")) return "Explanation";
  return "Overview";
}
function computeTitleSize(title, description) {
  const tl = title.length;
  const dl = description ? description.length : 0;
  const total = tl + dl;
  let size;
  if (tl <= 6) size = 190;
  else if (tl <= 12) size = 145;
  else if (tl <= 22) size = 108;
  else if (tl <= 35) size = 78;
  else if (tl <= 55) size = 60;
  else size = 46;
  if (total > 260) size = Math.min(size, 44);
  else if (total > 200) size = Math.min(size, 56);
  else if (total > 150) size = Math.min(size, 70);
  else if (total > 100) size = Math.min(size, 88);
  return size;
}
const PARTICLES = [
  // Dense cluster top-right (constellation near glow)
  { x: 1150, y: 42, r: 3, o: 0.55 },
  { x: 1108, y: 78, r: 2, o: 0.45 },
  { x: 1172, y: 122, r: 2, o: 0.4 },
  { x: 1068, y: 156, r: 4, o: 0.32 },
  { x: 1140, y: 194, r: 2, o: 0.5 },
  { x: 1080, y: 240, r: 3, o: 0.38 },
  { x: 1180, y: 278, r: 2, o: 0.42 },
  { x: 1008, y: 302, r: 2, o: 0.35 },
  { x: 1112, y: 346, r: 3, o: 0.3 },
  { x: 1160, y: 400, r: 2, o: 0.45 },
  // Trailing cluster middle-right (fades outward)
  { x: 940, y: 220, r: 2, o: 0.28 },
  { x: 880, y: 320, r: 2, o: 0.24 },
  { x: 820, y: 140, r: 2, o: 0.22 },
  { x: 760, y: 400, r: 3, o: 0.2 },
  { x: 700, y: 260, r: 2, o: 0.18 },
  { x: 1e3, y: 480, r: 2, o: 0.22 },
  { x: 1080, y: 540, r: 3, o: 0.25 },
  // Sparse left side (barely there)
  { x: 200, y: 120, r: 2, o: 0.15 },
  { x: 340, y: 480, r: 2, o: 0.15 }
];
const V_LINES = [108, 208, 308, 408, 508, 608, 708, 808, 908, 1008, 1108];
const H_LINES_TOP = [90, 180, 270];
const makeRender = ({ logoDataUrl, accentHex, accentHexA12, domain, projectName }) => {
  return async ({ title, description, pathname }) => {
    const titleSize = computeTitleSize(title, description);
    const section = getSection(pathname);
    const cleanPath = pathname === "/" ? "" : pathname.replace(/^\/|\/$/g, "");
    const is404 = section === "404";
    return /* @__PURE__ */ jsxs("div", { tw: "flex flex-col w-full h-full bg-white relative overflow-hidden", children: [
      /* @__PURE__ */ jsx("div", { tw: "absolute", style: { left: 0, top: 0, bottom: 0, width: 8, backgroundColor: accentHex } }),
      /* @__PURE__ */ jsx("div", { tw: `absolute top-0 right-0 w-full h-full bg-linear-to-bl from-[${accentHexA12}] via-transparent to-transparent` }),
      V_LINES.map((x) => /* @__PURE__ */ jsx("div", { style: { position: "absolute", left: x, top: 0, bottom: 0, width: 1, backgroundColor: "rgba(0,0,0,0.02)" } }, `v${x}`)),
      H_LINES_TOP.map((y) => /* @__PURE__ */ jsx("div", { style: { position: "absolute", top: y, left: 8, right: 0, height: 1, backgroundColor: "rgba(0,0,0,0.025)" } }, `h${y}`)),
      PARTICLES.map((p, i) => /* @__PURE__ */ jsx("div", { style: { position: "absolute", left: p.x, top: p.y, width: p.r * 2, height: p.r * 2, borderRadius: "50%", backgroundColor: hexToRgba(accentHex, p.o) } }, `p${i}`)),
      /* @__PURE__ */ jsxs("div", { tw: "flex flex-col h-full relative", style: { paddingTop: 60, paddingBottom: 48, paddingRight: 72, paddingLeft: 108, zIndex: 10 }, children: [
        /* @__PURE__ */ jsxs("div", { tw: "flex items-center justify-between", children: [
          /* @__PURE__ */ jsxs("div", { tw: "flex items-center", style: { gap: 20, alignItems: "center" }, children: [
            /* @__PURE__ */ jsx("img", { src: logoDataUrl, style: { width: 120, height: 120, objectFit: "contain", display: "block" }, alt: "" }),
            /* @__PURE__ */ jsx("span", { style: { fontSize: 40, fontWeight: 800, color: "#09090b", letterSpacing: "-0.045em", lineHeight: 1, marginTop: 4 }, children: projectName })
          ] }),
          /* @__PURE__ */ jsxs("div", { tw: "flex items-center", style: {
            borderWidth: 1,
            borderStyle: "solid",
            borderColor: hexToRgba(accentHex, 0.35),
            paddingLeft: 14,
            paddingRight: 18,
            paddingTop: 9,
            paddingBottom: 9,
            borderRadius: 4,
            gap: 10
          }, children: [
            /* @__PURE__ */ jsx("div", { style: { width: 7, height: 7, borderRadius: 3.5, backgroundColor: accentHex } }),
            /* @__PURE__ */ jsx("span", { style: { color: accentHex, fontSize: 12, fontWeight: 700, letterSpacing: "0.22em", textTransform: "uppercase" }, children: section })
          ] })
        ] }),
        /* @__PURE__ */ jsx("div", { tw: "flex-1" }),
        /* @__PURE__ */ jsxs("div", { tw: "flex flex-col", children: [
          /* @__PURE__ */ jsx("div", { style: { width: 60, height: 5, backgroundColor: accentHex, marginBottom: 20 } }),
          /* @__PURE__ */ jsx("h1", { style: { fontSize: titleSize, fontWeight: 800, lineHeight: 0.95, letterSpacing: "-0.05em", color: "#09090b", maxWidth: 1020, margin: 0 }, children: title }),
          description && !is404 && /* @__PURE__ */ jsx("p", { style: { fontSize: 21, lineHeight: 1.5, color: "#52525b", letterSpacing: "-0.008em", maxWidth: 880, margin: 0, marginTop: 18 }, children: description })
        ] }),
        /* @__PURE__ */ jsxs("div", { tw: "flex items-center justify-between", style: { marginTop: 28, paddingTop: 20, borderTopWidth: 1, borderTopStyle: "solid", borderTopColor: "rgba(0,0,0,0.14)" }, children: [
          /* @__PURE__ */ jsx("span", { style: { fontSize: 10, letterSpacing: "0.22em", color: "#a1a1aa", fontWeight: 700 }, children: "DOCUMENTATION" }),
          /* @__PURE__ */ jsxs("span", { style: { fontSize: 15, fontFamily: "monospace", color: "#71717a", letterSpacing: "-0.01em" }, children: [
            domain,
            cleanPath ? "/" + cleanPath : ""
          ] })
        ] }),
        /* @__PURE__ */ jsx("div", { style: { position: "absolute", bottom: 4, left: 108, fontSize: 7, color: "rgba(0,0,0,0.12)", fontFamily: "monospace" }, children: VERSION })
      ] })
    ] });
  };
};
export {
  makeRender
};
