// Snapshots an app's static UI from its live origin into the Android build,
// so the APK ships the whole frontend: the webview loads the REAL origin
// (API calls, cookies and streams stay natively same-origin) while
// MainActivity serves every snapshotted GET from these bundled bytes - the
// HTML never crosses the network. Run when apps/<name>/app.json says
// "snapshot": true; otherwise it clears any stale snapshot and exits.
//
// What gets collected, all same-origin GETs:
//   - "/" (the page itself)
//   - every href/src the HTML references
//   - the service worker's precache list when the app serves sw.js
//     (llm-chat-style `var SHELL = [...]`), plus sw.js itself
//   - manifest.webmanifest and its icons
//
//   node snapshot.mjs [app]     (or APP=<app>; default eyesoff)
import { readFileSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const APP = process.argv[2] || process.env.APP || "eyesoff";
const root = fileURLToPath(new URL(".", import.meta.url));
const outDir = join(root, "android/app/src/main/assets/appsnapshot");
const app = JSON.parse(readFileSync(new URL(`./apps/${APP}/app.json`, import.meta.url), "utf8"));

rmSync(outDir, { recursive: true, force: true });
if (!app.snapshot) {
  console.log(`apps/${APP} has no "snapshot": true - building without a bundled UI`);
  process.exit(0);
}

const ORIGIN = new URL(app.url).origin;
const MAX_FILES = 200;
const MAX_TOTAL = 40 * 1024 * 1024;
const MIME = {
  html: "text/html", js: "text/javascript", mjs: "text/javascript", css: "text/css",
  json: "application/json", webmanifest: "application/manifest+json",
  svg: "image/svg+xml", png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
  gif: "image/gif", webp: "image/webp", ico: "image/x-icon",
  woff2: "font/woff2", woff: "font/woff", ttf: "font/ttf", wasm: "application/wasm",
};

// same-origin path or null; queries dropped (the interceptor matches by path)
function pathOf(ref, base) {
  try {
    const u = new URL(ref, base);
    if (u.origin !== ORIGIN) return null;
    return u.pathname;
  } catch { return null; }
}

const wanted = new Set(["/"]);
const files = {};   // path -> { file, mime }
let total = 0, n = 0;

async function grab(path) {
  if (path in files) return null;
  const r = await fetch(ORIGIN + path, { redirect: "follow" });
  if (!r.ok) { console.log(`  skip ${path}: HTTP ${r.status}`); return null; }
  const bytes = Buffer.from(await r.arrayBuffer());
  if (n >= MAX_FILES || total + bytes.length > MAX_TOTAL) {
    console.log(`  skip ${path}: over the snapshot budget (${n} files, ${total} bytes) - NOT bundled`);
    return null;
  }
  const served = (r.headers.get("content-type") || "").split(";")[0].trim();
  const ext = (path.split(".").pop() || "").toLowerCase();
  const mime = served || MIME[ext] || "application/octet-stream";
  const file = "f" + n + (MIME[ext] ? "." + ext : "");
  writeFileSync(join(outDir, file), bytes);
  files[path === "" ? "/" : path] = { file, mime };
  total += bytes.length; n++;
  console.log(`  + ${path} (${bytes.length}b ${mime})`);
  return { bytes, mime };
}

mkdirSync(outDir, { recursive: true });
console.log(`snapshotting ${ORIGIN} for apps/${APP}`);

const page = await grab("/");
if (!page) throw new Error(`could not fetch ${ORIGIN}/ - is the app running?`);
const html = page.bytes.toString("utf8");

// references out of the page: href/src attributes and css url(...), plus the
// pwa entry points
for (const m of html.matchAll(/(?:href|src)\s*=\s*["']([^"']+)["']/gi)) {
  const p = pathOf(m[1], ORIGIN + "/");
  if (p && p !== "/") wanted.add(p);
}
for (const m of html.matchAll(/url\(\s*["']?([^"')]+)["']?\s*\)/gi)) {
  if (/^data:/i.test(m[1])) continue;
  const p = pathOf(m[1], ORIGIN + "/");
  if (p && p !== "/") wanted.add(p);
}
for (const p of ["/manifest.webmanifest", "/sw.js", "/favicon.ico", "/favicon.svg"]) wanted.add(p);

for (const p of [...wanted]) if (p !== "/") await grab(p).catch((e) => console.log(`  skip ${p}: ${e.message}`));

// the service worker's own precache list is the app's authoritative shell
if (files["/sw.js"]) {
  const sw = readFileSync(join(outDir, files["/sw.js"].file), "utf8");
  const list = sw.split("SHELL = [")[1]?.split("]")[0] || "";
  for (const m of list.matchAll(/"([^"]*)"/g)) {
    const p = pathOf(m[1], ORIGIN + "/");
    if (p && !(p in files)) await grab(p).catch((e) => console.log(`  skip ${p}: ${e.message}`));
  }
}
// the manifest's icons
if (files["/manifest.webmanifest"]) {
  try {
    const man = JSON.parse(readFileSync(join(outDir, files["/manifest.webmanifest"].file), "utf8"));
    for (const icon of man.icons || []) {
      const p = pathOf(icon.src, ORIGIN + "/manifest.webmanifest");
      if (p && !(p in files)) await grab(p).catch((e) => console.log(`  skip ${p}: ${e.message}`));
    }
  } catch (_) {}
}

writeFileSync(join(outDir, "manifest.json"), JSON.stringify({ origin: ORIGIN, files }, null, 1));
console.log(`snapshot: ${n} files, ${(total / 1024).toFixed(0)} KiB from ${ORIGIN}`);
