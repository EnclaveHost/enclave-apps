// Generates apps/dep-<label>/ for one RUNNING deployment, so the ordinary
// build pipeline (configure + brand + snapshot) turns it into a prepackaged,
// pre-linked APK with zero hand-written config. Branding comes from the live
// app itself - its web manifest's name, colors and largest icon - with the
// platform mark as the fallback for apps that publish none.
//
//   node gen-dep-app.mjs <0x-id-or-8-hex-label>
import { readFileSync, writeFileSync, mkdirSync, copyFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";

const raw = String(process.argv[2] || "").toLowerCase().replace(/^0x/, "");
if (!/^[0-9a-f]{8}([0-9a-f]{56})?$/.test(raw)) {
  throw new Error("pass a deployment id (0x…64 hex) or its 8-hex label");
}
const label = raw.slice(0, 8);
const origin = `https://${label}.app.enclave.host`;
const root = fileURLToPath(new URL(".", import.meta.url));
const dir = join(root, "apps", "dep-" + label);

const alive = await fetch(origin + "/", { redirect: "follow" }).catch((e) => ({ ok: false, err: e }));
if (!alive.ok) {
  throw new Error(`${origin} did not answer (${alive.status || alive.err}); only a running public deployment can be prepackaged`);
}

let name = label, theme = "#16181d", bg = "#16181d", iconFile = "icon.svg";
mkdirSync(dir, { recursive: true });
try {
  const man = await (await fetch(origin + "/manifest.webmanifest")).json();
  if (man.name || man.short_name) name = String(man.name || man.short_name).slice(0, 40);
  if (/^#[0-9a-fA-F]{3,8}$/.test(man.theme_color || "")) theme = man.theme_color;
  if (/^#[0-9a-fA-F]{3,8}$/.test(man.background_color || "")) bg = man.background_color;
  const icons = (man.icons || [])
    .map((i) => ({ ...i, px: parseInt(String(i.sizes || "0").split("x")[0], 10) || 0 }))
    .sort((a, b) => b.px - a.px);
  for (const icon of icons) {
    const u = new URL(icon.src, origin + "/manifest.webmanifest");
    if (u.origin !== origin) continue;
    const r = await fetch(u);
    if (!r.ok) continue;
    const ext = /svg/.test(r.headers.get("content-type") || "") || /\.svg$/i.test(u.pathname) ? "svg" : "png";
    writeFileSync(join(dir, "icon." + ext), Buffer.from(await r.arrayBuffer()));
    iconFile = "icon." + ext;
    break;
  }
} catch (_) {
  // no manifest: the platform look stands in
}
if (iconFile === "icon.svg") {
  try { readFileSync(join(dir, "icon.svg")); }
  catch { copyFileSync(join(root, "apps", "enclave", "icon.svg"), join(dir, "icon.svg")); }
}

const app = {
  name: "dep-" + label,
  displayName: name,
  // "a" + the hex label: always a valid java-style id segment, unique per
  // deployment so different apps install side by side
  appId: "host.enclave.a" + label,
  url: origin,
  attestationPath: "/attestation",
  repo: "EnclaveHost/enclave",
  icon: iconFile,
  backgroundColor: bg,
  themeColor: theme,
  iconBackground: bg,
  snapshot: true,
  _generated: "gen-dep-app.mjs - branding read from the live app's web manifest",
};
writeFileSync(join(dir, "app.json"), JSON.stringify(app, null, 2) + "\n");
console.log(`apps/dep-${label}: "${name}" wrapping ${origin} (icon ${iconFile}, theme ${theme})`);
