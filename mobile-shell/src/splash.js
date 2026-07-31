// The shell's entry page: verify the deployment, then hand the webview to the
// app. The splash is bundled and local - nothing it executes comes from the
// network - and the app origin only loads after a PASS (or an explicit,
// labeled user override; never silently).
import { Capacitor, CapacitorHttp } from "@capacitor/core";
import { verifyApp } from "./verify.mjs";
import cfg from "./config.gen.js";

const $ = (id) => document.getElementById(id);

// A PASS is remembered for a day so daily opens are instant; anything else is
// never cached. The record names url+repo+release so a config change or a
// version bump re-verifies.
const PASS_KEY = "enclave-shell-pass";
const PASS_TTL_MS = 24 * 60 * 60 * 1000;

// The app origin sends no CORS headers (yet), so in the webview this one
// cross-origin fetch rides the native HTTP bridge. The verifier's own fetches
// (GitHub, Sigstore, the enclave endpoint) are CORS-open - the platform site
// already runs it in a browser. Plain fetch stays as the fallback so the
// splash also works in a desktop browser once the app serves CORS.
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

function enter() {
  window.location.replace(cfg.url + "/");
}

function stage(text) {
  $("status").textContent = text;
}

function showFail(result) {
  $("verifying").hidden = true;
  $("failed").hidden = false;
  $("fail-reason").textContent = result.error || result.reason || "verification failed";
  const lines = Object.entries(result.steps || {}).map(([k, v]) => `${k}: ${v}`);
  $("fail-steps").textContent = lines.join("\n");
}

function recordPass(result) {
  try {
    localStorage.setItem(
      PASS_KEY,
      JSON.stringify({ url: cfg.url, repo: cfg.repo, release: result.release, at: Date.now() })
    );
  } catch (_) {}
}

function freshPass() {
  try {
    const p = JSON.parse(localStorage.getItem(PASS_KEY));
    if (p && p.url === cfg.url && p.repo === cfg.repo && Date.now() - p.at < PASS_TTL_MS) return p;
  } catch (_) {}
  return null;
}

async function run() {
  // Offline: there is nothing to verify against, and the app's own service
  // worker serves the cached shell. Say so and go.
  if (navigator.onLine === false) {
    stage("offline - opening the cached app");
    setTimeout(enter, 700);
    return;
  }

  const cached = freshPass();
  if (cached) {
    const hours = Math.max(1, Math.round((Date.now() - cached.at) / 3_600_000));
    stage(`enclave verified ${hours}h ago - opening`);
    setTimeout(enter, 500);
    return;
  }

  const result = await verifyApp({
    url: cfg.url,
    attestationPath: cfg.attestationPath,
    repo: cfg.repo,
    getJson,
    onStage: stage,
  });

  if (result.pass) {
    recordPass(result);
    const rel = result.release ? result.release.replace("sha256:", "").slice(0, 12) : "?";
    stage(`verified: release ${rel} on ${result.deployment.enclave || result.origin}`);
    setTimeout(enter, 900);
    return;
  }
  showFail(result);
}

$("retry").addEventListener("click", () => {
  $("failed").hidden = true;
  $("verifying").hidden = false;
  run().catch((e) => showFail({ error: String(e && e.message ? e.message : e), steps: {} }));
});

// Deliberate, labeled, and never remembered: an override opens the app this
// once without a verified deployment behind it.
$("enter-anyway").addEventListener("click", enter);

// Brand the static page from the bundled config: one page, every app.
document.title = cfg.displayName;
$("app-name").textContent = cfg.displayName;
if (cfg.backgroundColor) document.documentElement.style.setProperty("--bg", cfg.backgroundColor);
if (cfg.iconDataUri) {
  const mark = $("mark");
  mark.src = cfg.iconDataUri;
  mark.hidden = false;
}
run().catch((e) => showFail({ error: String(e && e.message ? e.message : e), steps: {} }));
