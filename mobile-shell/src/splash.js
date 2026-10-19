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
import { makeNativeFetch } from "./native-fetch.mjs";
import { referralFrom, referralCode, saveReferral, reportReferral } from "./referral.mjs";
import cfg from "./config.gen.js";

const $ = (id) => document.getElementById(id);

const PASS_KEY = "enclave-shell-pass";
const TARGET_KEY = "enclave-shell-target";
const PASS_TTL_MS = 24 * 60 * 60 * 1000;

// The relay grants CORS to enclave.host only, so in the webview every request the check makes (the verifier's own
// included) rides the native HTTP bridge (src/native-fetch.mjs); plain fetch stays as the fallback so the splash also
// loads in a desktop browser (where the relay's CORS refuses it).
const nativeFetch = Capacitor.isNativePlatform() ? makeNativeFetch(CapacitorHttp) : (url, opts) => fetch(url, opts);

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

// ---- referral code (src/referral.mjs): attribution only, never in the way --------
const randomHex = () => Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
async function postText(url, body) {
  if (Capacitor.isNativePlatform()) {
    const r = await CapacitorHttp.post({ url, headers: { "content-type": "text/plain" }, data: body });
    return { status: r.status };
  }
  const r = await fetch(url, { method: "POST", headers: { "content-type": "text/plain" }, body });
  return { status: r.status };
}
function reportSoon() {
  reportReferral({ storage: localStorage, post: postText, apiBase: cfg.apiBase, platform: Capacitor.getPlatform(),
                   app: cfg.appId || "", randomHex }).catch(() => {});
}
async function referralFromLaunch() {
  let code = referralFrom(window.location.href);
  if (!code && Capacitor.isNativePlatform()) {
    try { code = referralFrom((await App.getLaunchUrl())?.url); } catch (_) {}
  }
  if (code) saveReferral(localStorage, code);
  reportSoon();
}

// ---- ui states -------------------------------------------------------------
function show(state) {
  for (const id of ["verifying", "failed", "pairing"]) $(id).hidden = id !== state;
}
function stage(text) {
  $("status").textContent = text;
}
// A sign-in coming back through the /sso-app app link on a COLD start (the app
// was not running, or Android had killed it while the browser tab was open):
// the launch URL carries the token fragment, and the page needs it after the
// verify gate, on the same origin only. A running app gets it in MainActivity.
let signinFrag = "";
async function readSignin(target) {
  if (!Capacitor.isNativePlatform() || !target) return;
  try {
    const u = new URL((await App.getLaunchUrl())?.url || "");
    if (u.protocol === "https:" && u.origin === target.url && /^\/sso-app(\/|$)/.test(u.pathname) && /^#(.*&)?sso=/.test(u.hash))
      signinFrag = u.hash;
  } catch (_) {}
}
function enter(target) {
  window.location.replace(target.url + "/" + signinFrag);
}
function showFail(result) {
  show("failed");
  $("fail-reason").textContent = result.error || result.reason || "verification failed";
  const lines = Object.entries(result.steps || {}).map(([k, v]) => `${k}: ${v}`);
  if (result.deployment?.box) lines.unshift(`host: ${result.deployment.box}${result.deployment.mode ? ` (${result.deployment.mode})` : ""}`);
  $("fail-steps").textContent = lines.join("\n");
}

// ---- the trust cache: a PASS is remembered per target for a day ------------
function recordPass(target, result) {
  try {
    localStorage.setItem(
      PASS_KEY,
      JSON.stringify({ url: target.url, verifier: cfg.verifierSha256, deployment: result.deployment.id, at: Date.now() })
    );
  } catch (_) {}
}
function freshPass(target) {
  try {
    const p = JSON.parse(localStorage.getItem(PASS_KEY));
    if (p && p.url === target.url && p.verifier === cfg.verifierSha256 && Date.now() - p.at < PASS_TTL_MS) return p;
  } catch (_) {}
  return null;
}

// ---- the flow --------------------------------------------------------------
let current = null;

async function run(target) {
  current = target;
  await readSignin(target);
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
    deploymentId: target.id,
    apiBase: cfg.apiBase,
    fetchImpl: nativeFetch,
    onStage: stage,
  });

  if (result.pass) {
    recordPass(target, result);
    // "limited" is the most an unpinned check can say (the expected measurements are the relay's prediction)
    stage(`${result.status === "verified" ? "verified" : "verified (limited)"}: ${Object.values(result.steps).filter((x) => x === "pass").length} checks passed on ${result.deployment.box}`);
    setTimeout(() => enter(target), 900);
    return;
  }
  showFail(result);
}

function boot() {
  referralFromLaunch().catch(() => {});
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
  const ref = $("ref-in").value.trim();
  if (ref && !referralCode(ref)) {
    $("pair-err").textContent = "a referral code is 1 to 64 letters, digits, '.', '_' or '-'";
    return;
  }
  if (ref) { saveReferral(localStorage, ref); reportSoon(); }
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
