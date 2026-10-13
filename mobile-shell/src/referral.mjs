// A referral code this install came with: attribution only (the platform's relay/referrals.mjs). It arrives in a link
// (?r=<code> or ?ref=<code>, on the pairing deep link or a plain referral link) or is typed on the pairing screen, is
// kept in localStorage with a random install id, and is reported ONCE to <apiBase>/v1/referrals; a failed report is
// retried on a later launch. Nothing in the shell depends on it.
export const REFERRAL_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
const KEY = "enclave-shell-referral";
const ID_KEY = "enclave-shell-install";

/** A referral code, trimmed, or null when it is absent or not a code. */
export function referralCode(v) {
  const s = String(v ?? "").trim();
  return REFERRAL_RE.test(s) ? s : null;
}

/** The code a link carries (?r= or ?ref=), or null. */
export function referralFrom(urlStr) {
  try {
    const q = new URL(String(urlStr)).searchParams;
    return referralCode(q.get("r") ?? q.get("ref"));
  } catch {
    return null;
  }
}

function read(storage) {
  try { return JSON.parse(storage.getItem(KEY)) || null; } catch { return null; }
}

/** This install's stored code: {code, reported} or null. */
export const storedReferral = (storage) => read(storage);

/**
 * Keep a code for this install. Until it has been reported, a newer code replaces it (the owner corrected a typo);
 * once reported it is fixed here, as the relay keeps the first code it saw. -> true when stored.
 */
export function saveReferral(storage, code) {
  const c = referralCode(code);
  if (!c) return false;
  const cur = read(storage);
  if (cur?.reported) return cur.code === c;
  try { storage.setItem(KEY, JSON.stringify({ code: c, reported: false })); return true; } catch { return false; }
}

/** This install's random id (32 hex), made once. */
export function installId(storage, randomHex) {
  try {
    const have = storage.getItem(ID_KEY);
    if (/^[0-9a-f]{32}$/.test(have || "")) return have;
    const id = randomHex();
    storage.setItem(ID_KEY, id);
    return id;
  } catch {
    return randomHex();
  }
}

/**
 * Report the stored code if it has not been yet. post(url, bodyText) -> {status}. Only android/ios report (the relay
 * takes nothing else). -> "reported" | "nothing" | "skipped" | "failed".
 */
export async function reportReferral({ storage, post, apiBase, platform, app = "", randomHex }) {
  const cur = read(storage);
  if (!cur?.code || cur.reported) return "nothing";
  if (platform !== "android" && platform !== "ios") return "skipped";
  const body = JSON.stringify({ installId: installId(storage, randomHex), code: cur.code, platform, ...(app ? { app } : {}) });
  try {
    const r = await post(String(apiBase).replace(/\/+$/, "") + "/v1/referrals", body);
    if (r.status === 200 || r.status === 400) {
      // 400: the relay will never take this code; do not retry it on every launch
      storage.setItem(KEY, JSON.stringify({ ...cur, reported: true, ...(r.status === 400 ? { refused: true } : {}) }));
      return r.status === 200 ? "reported" : "failed";
    }
    return "failed";
  } catch {
    return "failed";
  }
}
