// Renders every native icon and splash for one app from apps/<name>/app.json
// (its icon file + colors), overwriting the committed eyesoff art in place -
// the same re-stamp model as configure.mjs. Needs rsvg-convert (librsvg) on
// PATH; CI installs librsvg2-bin.
//
// The icon file is embedded into wrapper SVGs as a data: URI, so an app may
// ship its mark as SVG or PNG and the same wrappers work:
//   - adaptive foreground: transparent, mark at 50% (the 66/108dp safe zone)
//   - round / full-bleed:  iconBackground under the mark at 76% / 86%
//   - splash:              backgroundColor under the mark at 25%, centered
//
//   node brand.mjs [app]     (or APP=<app>; default eyesoff)
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const APP = process.argv[2] || process.env.APP || "eyesoff";
const root = fileURLToPath(new URL(".", import.meta.url));
const dir = new URL(`./apps/${APP}/`, import.meta.url);
const app = JSON.parse(readFileSync(new URL("app.json", dir), "utf8"));

const iconBytes = readFileSync(new URL(app.icon, dir));
const iconMime = /\.svg$/i.test(app.icon) ? "image/svg+xml" : "image/png";
const iconUri = `data:${iconMime};base64,${iconBytes.toString("base64")}`;
const iconBg = app.iconBackground || app.backgroundColor || "#212121";
const splashBg = app.backgroundColor || "#212121";

const tmp = mkdtempSync(join(tmpdir(), "shell-brand-"));
const wrapper = (body) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" width="512" height="512">${body}</svg>`;
const mark = (scale) => {
  const s = Math.round(512 * scale), o = Math.round((512 - s) / 2);
  return `<image href="${iconUri}" x="${o}" y="${o}" width="${s}" height="${s}"/>`;
};
const svgs = {
  full: wrapper(`<rect width="512" height="512" fill="${iconBg}"/>` + mark(0.86)),
  fg: wrapper(mark(0.5)),
  round: wrapper(
    `<defs><clipPath id="r"><circle cx="256" cy="256" r="256"/></clipPath></defs>` +
    `<g clip-path="url(#r)"><rect width="512" height="512" fill="${iconBg}"/>` + mark(0.76) + `</g>`),
  plain: wrapper(mark(1)),
  splash: wrapper(`<rect width="512" height="512" fill="${splashBg}"/>` + mark(0.25)),
};
for (const [k, v] of Object.entries(svgs)) writeFileSync(join(tmp, k + ".svg"), v);

const render = (src, out, w, h, opts = []) =>
  execFileSync("rsvg-convert", ["-w", String(w), "-h", String(h || w), ...opts, join(tmp, src + ".svg"), "-o", join(root, out)]);
const pngSize = (p) => {
  const b = readFileSync(join(root, p));            // width/height straight from the IHDR
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
};

// android launcher icons
const DENS = { mdpi: 1, hdpi: 1.5, xhdpi: 2, xxhdpi: 3, xxxhdpi: 4 };
for (const [d, m] of Object.entries(DENS)) {
  const res = `android/app/src/main/res/mipmap-${d}/`;
  render("fg", res + "ic_launcher_foreground.png", 108 * m);
  render("plain", res + "ic_launcher.png", 48 * m);
  render("round", res + "ic_launcher_round.png", 48 * m);
}
writeFileSync(join(root, "android/app/src/main/res/values/ic_launcher_background.xml"),
  `<?xml version="1.0" encoding="utf-8"?>\n<resources>\n    <color name="ic_launcher_background">${iconBg}</color>\n</resources>`);

// android splashes, each at the committed file's own resolution
for (const d of ["", "-land-mdpi", "-land-hdpi", "-land-xhdpi", "-land-xxhdpi", "-land-xxxhdpi",
                 "-port-mdpi", "-port-hdpi", "-port-xhdpi", "-port-xxhdpi", "-port-xxxhdpi"]) {
  const p = `android/app/src/main/res/drawable${d}/splash.png`;
  const [w, h] = pngSize(p);
  const s = Math.min(w, h);
  render("splash", p, s, s, ["--page-width", String(w), "--page-height", String(h),
    "--left", String(Math.floor((w - s) / 2)), "--top", String(Math.floor((h - s) / 2)), "-b", splashBg]);
}

// ios: one 1024 icon (full-bleed - iOS masks it itself) + three 2732 splashes
render("full", "ios/App/App/Assets.xcassets/AppIcon.appiconset/AppIcon-512@2x.png", 1024);
for (const f of ["splash-2732x2732.png", "splash-2732x2732-1.png", "splash-2732x2732-2.png"])
  render("splash", "ios/App/App/Assets.xcassets/Splash.imageset/" + f, 2732);

rmSync(tmp, { recursive: true, force: true });
console.log(`branded native art for ${app.displayName} (bg ${splashBg}, icon bg ${iconBg})`);
