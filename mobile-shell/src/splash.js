// The shell's entry page: decide WHICH app this shell wraps, verify its
// deployment, then hand the webview over. The splash is bundled and local -
// nothing it executes comes from the network - and an app origin only loads
// after a PASS (or an explicit, labeled user override; never silently).
//
// Which app: a branded build carries its app in config; the GENERIC shell
// (config.generic) learns it at runtime - from the pairing deep link the
// dashboard mints (<scheme>://open?u=<origin>&d=<id>&n=<name>, or ?u= on the
// page URL), from the stored previous pairing, or from the pairing screen.
// A pairing target must clear the same allowNavigation list the webview
// enforces, so the shell can never be linked into wrapping a foreign site.
import { Capacitor, CapacitorHttp } from "@capacitor/core";
import { App } from "@capacitor/app";
import { verifyApp } from "./verify.mjs";
import cfg from "./config.gen.js";

const $ = (id) => document.getElementById(id);

const PASS_KEY = "enclave-shell-pass";
const TARGET_KEY = "enclave-shell-target";
const PASS_TTL_MS = 24 * 60 * 60 * 1000;

// The app origin may send no CORS headers, so in the webview cross-origin
// fetches ride the native HTTP bridge. The verifier's own fetches (GitHub,
// Sigstore, the enclave endpoint) are CORS-open. Plain fetch stays as the
// fallback so the splash also works in a desktop browser.
async function getJson(url) {
  if (Capacitor.isNativePlatform()) {
    const r = await CapacitorHttp.get({ url, headers: { accept: "application/json" }, responseType: "json" });
    if (r.status !== 200) throw new Error(`HTTP ${r.status}`);
    return typeof r.data === "string" ? JSON.parse(r.data) : r.data;
  }
  const r = await fetch(url, { headers: { accept: "application/json" } });
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  return r.json();
}

// ---- which app -------------------------------------------------------------
function hostAllowed(host) {
  const h = String(host || "").toLowerCase();
  return (cfg.allowNavigation || []).some((pat) => {
    const p = String(pat).toLowerCase();
    return p.startsWith("*.") ? h.endsWith(p.slice(1)) && h.length > p.length - 1 : h === p;
  });
}

// A target is {url, id, name} and only ever built here: https, an allowed
// host, a well-formed 0x id or none. The name is display-only (textContent).
function sanitizeTarget(u, d, n) {
  try {
    const url = new URL(String(u));
    if (url.protocol !== "https:" || !hostAllowed(url.host)) return null;
    return {
      url: url.origin,
      id: /^0x[0-9a-f]{64}$/i.test(String(d || "")) ? String(d).toLowerCase() : null,
      name: String(n || "").slice(0, 64) || url.host,
    };
  } catch {
    return null;
  }
}

function paramsOf(urlStr) {
  try {
    const q = new URL(String(urlStr)).searchParams;
    return { u: q.get("u"), d: q.get("d"), n: q.get("n") };
  } catch {
    return {};
  }
}

function storeTarget(t) {
  try { localStorage.setItem(TARGET_KEY, JSON.stringify({ ...t, at: Date.now() })); } catch (_) {}
}

async function resolveTarget() {
  // 1. an explicit link wins: the page URL's ?u= (web / testing), else the
  //    native launch URL (the pairing deep link on a cold start)
  let p = paramsOf(window.location.href);
  if (!p.u && Capacitor.isNativePlatform()) {
    try { p = paramsOf((await App.getLaunchUrl())?.url); } catch (_) {}
  }
  if (p.u) {
    const t = sanitizeTarget(p.u, p.d, p.n);
    if (t) { storeTarget(t); return t; }
  }
  // 2. the stored pairing
  try {
    const s = JSON.parse(localStorage.getItem(TARGET_KEY));
    const t = s && sanitizeTarget(s.url, s.id, s.name);
    if (t) return t;
  } catch (_) {}
  // 3. a branded build wraps its own app; the generic shell asks
  return cfg.generic ? null : { url: cfg.url, id: null, name: cfg.displayName };
}

// ---- ui states -------------------------------------------------------------
function show(state) {
  for (const id of ["verifying", "failed", "pairing"]) $(id).hidden = id !== state;
}
function stage(text) {
  $("status").textContent = text;
}
function enter(target) {
  window.location.replace(target.url + "/");
}
function showFail(result) {
  show("failed");
  $("fail-reason").textContent = result.error || result.reason || "verification failed";
  const lines = Object.entries(result.steps || {}).map(([k, v]) => `${k}: ${v}`);
  $("fail-steps").textContent = lines.join("\n");
}

// ---- the trust cache: a PASS is remembered per target for a day ------------
function recordPass(target, result) {
  try {
    localStorage.setItem(
      PASS_KEY,
      JSON.stringify({ url: target.url, repo: cfg.repo, release: result.release, at: Date.now() })
    );
  } catch (_) {}
}
function freshPass(target) {
  try {
    const p = JSON.parse(localStorage.getItem(PASS_KEY));
    if (p && p.url === target.url && p.repo === cfg.repo && Date.now() - p.at < PASS_TTL_MS) return p;
  } catch (_) {}
  return null;
}

// ---- the flow --------------------------------------------------------------
let current = null;

async function run(target) {
  current = target;
  show("verifying");
  $("app-name").textContent = target.name || cfg.displayName;

  if (navigator.onLine === false) {
    // nothing to verify against; the app's own service worker serves offline
    stage("offline - opening the cached app");
    setTimeout(() => enter(target), 700);
    return;
  }
  const cached = freshPass(target);
  if (cached) {
    const hours = Math.max(1, Math.round((Date.now() - cached.at) / 3_600_000));
    stage(`enclave verified ${hours}h ago - opening`);
    setTimeout(() => enter(target), 500);
    return;
  }

  const result = await verifyApp({
    url: target.url,
    attestationPath: cfg.attestationPath,
    repo: cfg.repo,
    deploymentId: target.id,
    apiBase: cfg.apiBase,
    getJson,
    onStage: stage,
  });

  if (result.pass) {
    recordPass(target, result);
    const rel = result.release ? result.release.replace("sha256:", "").slice(0, 12) : "?";
    stage(`verified: release ${rel} on ${result.deployment.enclave || result.origin}`);
    setTimeout(() => enter(target), 900);
    return;
  }
  showFail(result);
}

function boot() {
  resolveTarget()
    .then((t) => (t ? run(t) : show("pairing")))
    .catch((e) => showFail({ error: String(e && e.message ? e.message : e), steps: {} }));
}

// ---- wiring ----------------------------------------------------------------
$("retry").addEventListener("click", () => {
  if (current) run(current).catch((e) => showFail({ error: String(e?.message || e), steps: {} }));
  else boot();
});

// Deliberate, labeled, and never remembered as a pass: an override opens the
// app this once without a verified deployment behind it.
$("enter-anyway").addEventListener("click", () => current && enter(current));

// generic shell only: back out of a broken pairing to pick another app
if (cfg.generic) {
  $("switch-app").hidden = false;
  $("switch-app").addEventListener("click", () => {
    try { localStorage.removeItem(TARGET_KEY); } catch (_) {}
    current = null;
    show("pairing");
  });
}

$("pair-go").addEventListener("click", () => {
  const raw = $("pair-in").value.trim();
  const t = sanitizeTarget(/^https:\/\//i.test(raw) ? raw : "https://" + raw);
  if (!t) {
    $("pair-err").textContent = "that is not an app origin this shell can wrap (expected <label>.app.enclave.host)";
    return;
  }
  $("pair-err").textContent = "";
  storeTarget(t);
  run(t).catch((e) => showFail({ error: String(e?.message || e), steps: {} }));
});

// Brand the static page from the bundled config: one page, every app.
document.title = cfg.displayName;
$("app-name").textContent = cfg.displayName;
if (cfg.backgroundColor) document.documentElement.style.setProperty("--bg", cfg.backgroundColor);
if (cfg.iconDataUri) {
  const mark = $("mark");
  mark.src = cfg.iconDataUri;
  mark.hidden = false;
}
boot();
