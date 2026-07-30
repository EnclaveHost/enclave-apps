//! Vision by DELEGATION: the pictures go to a sibling `image-reader` deployment
//! and come back as prose, which this app folds into the turn it is about to
//! answer. The other half of vision - the model reading the image ITSELF, on a
//! volume with its own projector - is in lib.rs and unchanged; this module is
//! the path for a deployment whose chat model is bigger than any VLM it could
//! afford to attach beside it.
//!
//! WHY DELEGATE AT ALL, given the app can already do it in-process: because the
//! two capabilities have different shapes. The chat model is the deployment's
//! most expensive resident and is chosen for reasoning; a VLM is idle most of
//! the time and wants its own share, its own funding rate and its own restart
//! button. Splitting them means the eyes can be started, stopped, resized or
//! upgraded without touching the chat everyone is using - and it means the chat
//! deployment does not have to hold a second set of weights and a second dense
//! KV window in the same GPU share.
//!
//! WHAT CROSSES, and this is the whole design question: NOT the conversation.
//! One request carries the image bytes, a QUESTION, and optionally one paragraph
//! of context - and the question is written by THIS deployment's own model,
//! which has read the conversation and knows what matters about the picture.
//! That is what keeps a delegated look from being a lossy one: a generic
//! "describe this image" would have to guess what the user cares about before
//! anyone asked, whereas "check whether this screenshot has an email field, a
//! password field and a 'Forgot password' link" carries the relevant part of the
//! spec the user pasted three turns ago, without shipping the spec.
//!
//! WHAT IT COSTS, honestly: one extra generation (the query, ~60 tokens, greedy)
//! plus one round trip, and the look is SINGLE-SHOT. The chat model writes its
//! question before it knows what is in the picture, and there is no second look
//! inside one turn - the render path is one pass, not a tool-call loop. The
//! mitigation is in the prompt: the question asks for the specific answer AND
//! enough surrounding description to handle the obvious follow-up. When that is
//! not enough the user asks again, and the next turn re-queries with their new
//! question, which is an extra turn rather than a wrong answer.
//!
//! PRIVACY: the sibling deployment is inside the same confidential fabric - its
//! own enclave, its own attestation, and (see the image-reader app) a component
//! with no outbound socket at all, so what it is shown cannot be forwarded
//! anywhere. That is a very different bargain from the web-search leg, where the
//! query genuinely leaves for a third party. It is still a boundary: the image
//! and the model-written question cross it, and the answer comes back.
//!
//! REACHABILITY, as ever on this fleet: outbound egress is IPv6-only. Another
//! Enclave deployment is fine (every deployment gets a dedicated IPv6); an
//! IPv4-only host is not dialable at all.

use serde::Deserialize;

use crate::http::{self, HttpReq};

#[derive(Deserialize, Clone)]
pub struct VisionConfig {
    /// base URL of the vision service, e.g. an image-reader deployment
    /// "https://<id8>.app.enclave.host". A full path ending in /v1/vision is
    /// also accepted and used verbatim.
    pub endpoint: String,
    /// Bearer credential, when that deployment sets one. Reference a secret by
    /// name (`"$VISION_API_KEY"`), never the literal - app config is published
    /// on-chain by CID.
    #[serde(default)]
    pub api_key: Option<String>,
    /// model name from the vision service's catalog; absent = its default
    #[serde(default)]
    pub model: Option<String>,
    /// A cold VLM session plus an encoder pass on a shared H200 is seconds, and
    /// it can queue behind another tenant - but this sits on a chat turn's
    /// critical path, so the ceiling is minutes rather than the tens of minutes
    /// image generation gets.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// hard cap on the response. It is prose, not pixels: 512 KB is a very long
    /// description and still small enough that a runaway response cannot
    /// exhaust guest memory.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// how many of the turn's images to send. Each one is an encoder pass and a
    /// slice of the vision model's window; two is enough for "compare these".
    #[serde(default = "default_max_images")]
    pub max_images: usize,
    /// characters of the answer folded into the prompt. A description that ran
    /// long should not be able to push the conversation out of the window.
    #[serde(default = "default_max_answer_chars")]
    pub max_answer_chars: usize,
    /// Have THIS deployment's model write the question first (the default). Off
    /// sends the user's own words, which costs one generation less and is the
    /// right trade when the user's message is already a direct question about
    /// the picture and never refers to earlier turns.
    #[serde(default = "default_true")]
    pub author_query: bool,
    /// When the SELECTED chat model can read images itself, let it, instead of
    /// delegating. Off (the default) delegates anyway, which is usually right:
    /// the sibling exists so the big model does not have to be a VLM. A request
    /// that NAMES a vision model explicitly always keeps the picture local,
    /// whatever this says - an explicit choice beats a default.
    #[serde(default)]
    pub prefer_local: bool,
}

fn default_timeout_s() -> u64 {
    120
}
fn default_max_bytes() -> usize {
    512 * 1024
}
fn default_max_images() -> usize {
    2
}
fn default_max_answer_chars() -> usize {
    6000
}
fn default_true() -> bool {
    true
}

impl VisionConfig {
    /// The credential, or None if there isn't a usable one - including the
    /// `"$VISION_API_KEY"`-with-no-secret-set case, which would otherwise send
    /// the literal placeholder as a Bearer token and earn a baffling 401.
    fn key(&self) -> Option<&str> {
        let k = self.api_key.as_deref()?.trim();
        if k.is_empty() || is_unresolved_ref(k) {
            return None;
        }
        Some(k)
    }

    fn unresolved_key_name(&self) -> Option<&str> {
        let k = self.api_key.as_deref()?.trim();
        if !is_unresolved_ref(k) {
            return None;
        }
        Some(k.trim_start_matches('$').trim_start_matches('{').trim_end_matches('}'))
    }

    /// The vision URL. Accepts a bare origin (the normal case) or a full
    /// endpoint path, so an operator who pastes either gets a working config.
    fn url(&self) -> String {
        let base = self.endpoint.trim().trim_end_matches('/');
        if base.contains("/v1/vision") || base.ends_with("/vision") {
            base.to_string()
        } else {
            format!("{base}/v1/vision")
        }
    }
}

fn is_unresolved_ref(s: &str) -> bool {
    let Some(r) = s.strip_prefix('$') else { return false };
    let name = r.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(r);
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub struct VisionAnswer {
    /// what the vision model said, trimmed and capped at max_answer_chars
    pub text: String,
    /// the question it was actually asked (the model-written one, when that is
    /// what was sent) - reported so a user can see what was asked on their
    /// behalf, which matters more here than it does for a search query
    pub question: String,
    pub model: Option<String>,
    pub images: usize,
    /// positions the pictures cost the VISION model's window, its own figure
    pub image_tokens: usize,
    pub ms: u64,
    /// the answer was cut at max_answer_chars
    pub truncated: bool,
}

/// Ask the vision service one question about one turn's images.
///
/// Errors carry the service's own message where there is one, because "vision
/// failed" alone tells an operator nothing about whether the deployment is
/// unfunded, the key wrong, the node too old to see, or the picture too large.
pub fn describe(
    cfg: &VisionConfig,
    images: &[Vec<u8>],
    question: &str,
    context: Option<&str>,
    now_ms: impl Fn() -> u64,
    on_status: &dyn Fn(&str),
) -> Result<VisionAnswer, String> {
    if cfg.endpoint.trim().is_empty() {
        return Err("vision_service.endpoint is not set".into());
    }
    if images.is_empty() {
        return Err("no image to look at".into());
    }
    let question = question.trim();
    let uris: Vec<String> = images
        .iter()
        .take(cfg.max_images.max(1))
        .map(|b| format!("data:{};base64,{}", mime_of(b), b64_encode(b)))
        .collect();
    let sent = uris.len();
    let mut payload = serde_json::json!({ "images": uris });
    if !question.is_empty() {
        payload["question"] = serde_json::json!(question);
    }
    if let Some(c) = context.map(str::trim).filter(|c| !c.is_empty()) {
        payload["context"] = serde_json::json!(c);
    }
    if let Some(m) = cfg.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        payload["model"] = serde_json::json!(m);
    }
    let body = payload.to_string();
    let url = cfg.url();
    let mut req = HttpReq::post(&url, body.as_bytes())
        .timeout(cfg.timeout_s)
        .max_bytes(cfg.max_bytes)
        .header("content-type", b"application/json")
        .header("accept", b"application/json");
    match cfg.key() {
        Some(k) => req = req.header("authorization", format!("Bearer {k}").as_bytes()),
        None => {
            if let Some(name) = cfg.unresolved_key_name() {
                return Err(format!(
                    "vision_service.api_key references ${name} but no such secret is set on \
                     this deployment - add {name} to the deployment's secrets in the console \
                     (or set_secrets), then restart it to apply"
                ));
            }
        }
    }

    let t0 = now_ms();
    // Same heartbeat story as image::generate: the vision service answers in
    // one blob when the read is done, and a large picture on a busy share can
    // hold the wire silent long enough for an idle-timeout in the middle to
    // kill the stream. Tick every 15s until the first response byte.
    let r = http::request_with_tick(req, 15, &mut |s| {
        on_status(&format!("still reading the image… ({s}s)"))
    })?;
    if r.truncated {
        return Err(format!(
            "the vision service's reply exceeded vision_service.max_bytes ({} bytes)",
            cfg.max_bytes
        ));
    }
    if r.status != 200 {
        // the sibling app tags its errors with a machine-readable code and a
        // sentence naming what to change; pass both through rather than
        // flattening them into "HTTP 400"
        let v: Option<serde_json::Value> = serde_json::from_slice(&r.body).ok();
        let msg = v
            .as_ref()
            .and_then(|v| v["error"]["message"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                String::from_utf8_lossy(&r.body).trim().chars().take(300).collect()
            });
        let code = v.as_ref().and_then(|v| v["error"]["code"].as_str()).unwrap_or("");
        return Err(match code {
            "" => format!("the vision service returned HTTP {}: {msg}", r.status),
            c => format!("the vision service returned HTTP {} [{c}]: {msg}", r.status),
        });
    }
    let v: serde_json::Value = serde_json::from_slice(&r.body)
        .map_err(|e| format!("the vision service sent invalid JSON: {e}"))?;
    let raw = v["answer"].as_str().unwrap_or("").trim();
    if raw.is_empty() {
        return Err("the vision service returned an empty answer".into());
    }
    let (text, truncated) = cap_chars(raw, cfg.max_answer_chars);
    Ok(VisionAnswer {
        text,
        question: v["question"].as_str().unwrap_or(question).to_string(),
        model: v["model"].as_str().map(str::to_string).or_else(|| cfg.model.clone()),
        images: v["images"].as_u64().unwrap_or(sent as u64) as usize,
        image_tokens: v["image_tokens"].as_u64().unwrap_or(0) as usize,
        ms: now_ms().saturating_sub(t0),
        truncated,
    })
}

/// How the answer is presented to the chat model. Framed as an OBSERVATION
/// REPORT rather than as the user's words, and with the picture's own limits
/// stated, because the failure mode to design against is the chat model
/// treating a description as if it had seen the image and then answering a
/// follow-up ("what colour is the third icon?") out of thin air.
pub fn render_context(a: &VisionAnswer) -> String {
    let n = a.images;
    let subject = if n == 1 { "an image".to_string() } else { format!("{n} images") };
    format!(
        "[VISION REPORT. The user attached {subject}. You cannot see {} yourself; a vision \
         model in this same confidential deployment looked at {} and answered the question \
         below. Treat this report as your only evidence about the picture: use it freely, but \
         if the user asks about a detail the report does not mention, say that you would need \
         to look again rather than inventing it.\n\nQuestion asked of the vision model: \
         {}\n\nWhat it saw:\n{}{}]",
        if n == 1 { "it" } else { "them" },
        if n == 1 { "it" } else { "them" },
        a.question.trim(),
        a.text.trim(),
        if a.truncated { "\n[report truncated]" } else { "" }
    )
}

/// Cap a string at `max` CHARACTERS (not bytes), on a char boundary.
fn cap_chars(s: &str, max: usize) -> (String, bool) {
    if max == 0 || s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

/// The mime type, by magic bytes. The sibling sniffs the bytes itself and does
/// not trust this, but a data: URI has to carry something, and carrying the
/// truth costs nothing.
fn mime_of(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        "image/webp"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else if b.starts_with(b"BM") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

/// Standard base64, padded. The app already decodes it (lib.rs); this is the
/// other direction, needed because the picture arrived as bytes and leaves as a
/// data: URI. No dependency for twenty lines.
fn b64_encode(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((b.len() + 2) / 3 * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str, key: Option<&str>) -> VisionConfig {
        VisionConfig {
            endpoint: endpoint.into(),
            api_key: key.map(str::to_string),
            model: None,
            timeout_s: 120,
            max_bytes: 512 * 1024,
            max_images: 2,
            max_answer_chars: 6000,
            author_query: true,
            prefer_local: false,
        }
    }

    #[test]
    fn endpoint_accepts_origin_or_full_path() {
        assert_eq!(cfg("https://a.app.enclave.host", None).url(),
                   "https://a.app.enclave.host/v1/vision");
        assert_eq!(cfg("https://a.app.enclave.host/", None).url(),
                   "https://a.app.enclave.host/v1/vision");
        assert_eq!(cfg("https://a.app.enclave.host/v1/vision", None).url(),
                   "https://a.app.enclave.host/v1/vision");
    }

    #[test]
    fn unresolved_secret_is_not_a_bearer_token() {
        assert_eq!(cfg("https://x", Some("$VISION_API_KEY")).key(), None);
        assert_eq!(cfg("https://x", Some("${VISION_API_KEY}")).key(), None);
        assert_eq!(cfg("https://x", Some("")).key(), None);
        assert_eq!(cfg("https://x", Some("sk-real")).key(), Some("sk-real"));
        assert_eq!(cfg("https://x", Some("$VISION_API_KEY")).unresolved_key_name(),
                   Some("VISION_API_KEY"));
    }

    #[test]
    fn base64_matches_the_decoder_in_lib() {
        // round-trips through the app's own decoder, which is the only
        // compatibility that matters
        for case in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\x89PNG\r\n\x1a\n\x00\x01\x02"] {
            let enc = b64_encode(case);
            assert_eq!(enc.len() % 4, 0, "padding: {enc}");
            if case.is_empty() {
                continue;
            }
            assert_eq!(crate::b64_decode(&enc).unwrap(), case.to_vec(), "{enc}");
        }
    }

    #[test]
    fn mime_follows_the_bytes() {
        assert_eq!(mime_of(&[0x89, b'P', b'N', b'G', 0, 0]), "image/png");
        assert_eq!(mime_of(&[0xff, 0xd8, 0xff, 0]), "image/jpeg");
        assert_eq!(mime_of(b"RIFF____WEBP___"), "image/webp");
        assert_eq!(mime_of(b"nonsense"), "application/octet-stream");
    }

    #[test]
    fn a_long_report_is_capped_on_a_char_boundary() {
        let s = "é".repeat(10);
        let (t, cut) = cap_chars(&s, 4);
        assert!(cut);
        assert_eq!(t.chars().count(), 4);
        let (t, cut) = cap_chars("short", 100);
        assert!(!cut);
        assert_eq!(t, "short");
    }

    #[test]
    fn the_report_tells_the_model_it_did_not_see_the_picture() {
        let a = VisionAnswer {
            text: "A login form with two fields.".into(),
            question: "does it have a password field?".into(),
            model: Some("qwen3-vl-8b".into()),
            images: 1,
            image_tokens: 258,
            ms: 900,
            truncated: false,
        };
        let r = render_context(&a);
        assert!(r.contains("cannot see it yourself"));
        assert!(r.contains("does it have a password field?"));
        assert!(r.contains("A login form with two fields."));
        assert!(!r.contains("[report truncated]"));
        // plural reads correctly too
        let two = VisionAnswer { images: 2, ..a };
        assert!(render_context(&two).contains("2 images"));
        assert!(render_context(&two).contains("cannot see them yourself"));
    }
}
