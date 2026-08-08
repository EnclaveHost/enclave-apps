//! Image generation, done BY THE APP against an image-generator deployment.
//!
//! Same shape and the same reasoning as the web-search leg (search.rs): the
//! browser talks only to this app, and this app calls the image service from
//! the deployment's own egress identity. The prompt is part of the
//! conversation, so shipping it out of the browser to a third origin would
//! undo the point of running the model in an enclave.
//!
//! The intended target is the sibling `image-generator` app, which serves an
//! OpenAI-shaped `POST /v1/images/generations` returning base64 PNGs and
//! enforces `Authorization: Bearer` when its config sets an api_key. Any
//! service with that contract works; point `endpoint` at it.
//!
//! REACHABILITY, as ever on this fleet: outbound egress is IPv6-only. An
//! image-generator running as another Enclave deployment is fine, because
//! every deployment gets a dedicated IPv6 - it is third-party IPv4-only
//! hosts that cannot be dialled (see http::egress_err).
//!
//! The bytes come back to the browser INLINE, as a data: URI in an SSE event.
//! That keeps the image inside the same origin the chat already trusts and
//! means a stored conversation still renders offline, at the cost of carrying
//! ~1.4x the PNG through the stream and into IndexedDB - which is why
//! `max_bytes` is a real limit and not a formality.

use serde::Deserialize;

use crate::http::{self, HttpReq};

#[derive(Deserialize, Clone)]
pub struct ImageConfig {
    /// base URL of the image service, e.g. the image-generator deployment
    /// "https://<id8>.app.enclave.host". A full path ending in
    /// /v1/images/generations is also accepted and used verbatim.
    pub endpoint: String,
    /// Bearer credential, when the image deployment sets one. Reference a
    /// secret by name (`"$IMAGE_API_KEY"`), never the literal - App config is
    /// published on-chain by CID.
    #[serde(default)]
    pub api_key: Option<String>,
    /// model name from the image service's catalog; absent = its default
    /// (the largest attached model)
    #[serde(default)]
    pub model: Option<String>,
    /// "1024x1024" and friends. The service snaps sizes to its own limits.
    #[serde(default = "default_size")]
    pub size: String,
    /// Generation is slow - a 20B MMDiT at 8 steps on a shared H200 is tens
    /// of seconds, and it queues behind other tenants. This is the one
    /// timeout in the app that is measured in minutes.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// hard cap on the response, bytes. A 1024px PNG is ~1-2 MB and arrives
    /// base64'd (~1.4x) inside JSON; 12 MB leaves room for 2048px without
    /// letting a runaway response exhaust guest memory.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// whether the playground's image switch STARTS on. TRUE by default, unlike
    /// web search, and the difference is the disclosure rather than the
    /// principle. A search sends the user's question to a THIRD PARTY who keeps
    /// their own logs; an image prompt goes to a deployment this operator runs,
    /// inside the same fleet, under the same attestation. Configuring an
    /// endpoint is the opt-in, and there is no second party to warn about.
    ///
    /// An operator who points `endpoint` at something they do NOT run should
    /// set this false and let the user choose, the way search does.
    #[serde(default = "default_on")]
    pub default_on: bool,
}

fn default_on() -> bool {
    true
}

fn default_size() -> String {
    "1024x1024".into()
}
fn default_timeout_s() -> u64 {
    180
}
fn default_max_bytes() -> usize {
    12 * 1024 * 1024
}

impl ImageConfig {
    /// The credential, or None if there isn't a usable one - including the
    /// `"$IMAGE_API_KEY"`-with-no-secret-set case, which would otherwise send
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

    /// The generations URL. Accepts a bare origin (the normal case) or a full
    /// endpoint path, so an operator who pastes either gets a working config.
    fn url(&self) -> String {
        let base = self.endpoint.trim().trim_end_matches('/');
        if base.contains("/v1/images/generations") || base.ends_with("/generate") {
            base.to_string()
        } else {
            format!("{base}/v1/images/generations")
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

pub struct GeneratedImage {
    /// base64 image bytes, exactly as the service returned them
    pub b64: String,
    /// what the bytes are ("image/png" unless the service said otherwise via
    /// a data URI - see tools::image_payload)
    pub mime: String,
    pub prompt: String,
    pub model: Option<String>,
    pub seed: Option<i64>,
    pub ms: u64,
}

impl GeneratedImage {
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.b64)
    }
}

/// Generate one image. Errors carry the service's own message where there is
/// one, because "image generation failed" alone tells an operator nothing
/// about whether the model is unattached, the share too small, or the key wrong.
pub fn generate(
    cfg: &ImageConfig,
    prompt: &str,
    now_ms: impl Fn() -> u64,
    on_status: &dyn Fn(&str),
) -> Result<GeneratedImage, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("empty image prompt".into());
    }
    if cfg.endpoint.trim().is_empty() {
        return Err("image.endpoint is not set".into());
    }
    let mut payload = serde_json::json!({ "prompt": prompt, "n": 1, "size": cfg.size });
    if let Some(m) = &cfg.model {
        if !m.trim().is_empty() {
            payload["model"] = serde_json::json!(m);
        }
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
                    "image.api_key references ${name} but no such secret is set on this \
                     deployment - add {name} to the deployment's secrets in the console \
                     (or set_secrets), then restart it to apply"
                ));
            }
        }
    }

    let t0 = now_ms();
    // The service answers only when the picture is DONE - tens of seconds to
    // minutes of nothing on the wire, during which every idle-timeout between
    // this app and the user's client is entitled to kill the (until now
    // silent) stream. Measured on a live deployment 2026-07-30: gaps up to
    // 58.5s against the common 60s proxy default. The heartbeat closes that
    // window: on_status reaches the client as an SSE status event (/chat) or
    // comment (/v1 streaming) every 15s until the first response byte.
    let r = http::request_with_tick(req, 15, &mut |s| {
        on_status(&format!("still generating the image… ({s}s)"))
    })?;
    if r.truncated {
        return Err(format!(
            "image response exceeded image.max_bytes ({} bytes) - raise it, or ask the \
             service for a smaller size than {}",
            cfg.max_bytes, cfg.size
        ));
    }
    if r.status != 200 {
        let hint = String::from_utf8_lossy(&r.body);
        let hint = hint.trim();
        let hint: String = hint.chars().take(300).collect();
        return Err(format!("image service returned HTTP {}: {hint}", r.status));
    }
    let v: serde_json::Value = serde_json::from_slice(&r.body)
        .map_err(|e| format!("image service sent invalid JSON: {e}"))?;
    // OpenAI shape: { data: [ { b64_json, seed? } ] }
    let first = v["data"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or("image service returned no images")?;
    let b64 = first["b64_json"]
        .as_str()
        .ok_or("image service returned no b64_json")?
        .to_string();
    if b64.is_empty() {
        return Err("image service returned an empty image".into());
    }
    Ok(GeneratedImage {
        b64,
        mime: "image/png".into(),
        prompt: prompt.to_string(),
        model: v["model"].as_str().or(cfg.model.as_deref()).map(str::to_string),
        seed: first["seed"].as_i64(),
        ms: now_ms().saturating_sub(t0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(endpoint: &str, key: Option<&str>) -> ImageConfig {
        ImageConfig {
            endpoint: endpoint.into(),
            api_key: key.map(str::to_string),
            model: None,
            size: "1024x1024".into(),
            timeout_s: 180,
            max_bytes: 12 * 1024 * 1024,
            default_on: true,
        }
    }

    #[test]
    fn endpoint_accepts_origin_or_full_path() {
        assert_eq!(cfg("https://a.app.enclave.host", None).url(),
                   "https://a.app.enclave.host/v1/images/generations");
        // trailing slash, and an already-complete path, both land right
        assert_eq!(cfg("https://a.app.enclave.host/", None).url(),
                   "https://a.app.enclave.host/v1/images/generations");
        assert_eq!(cfg("https://a.app.enclave.host/v1/images/generations", None).url(),
                   "https://a.app.enclave.host/v1/images/generations");
    }

    #[test]
    fn unresolved_secret_is_not_a_bearer_token() {
        assert_eq!(cfg("https://x", Some("$IMAGE_API_KEY")).key(), None);
        assert_eq!(cfg("https://x", Some("${IMAGE_API_KEY}")).key(), None);
        assert_eq!(cfg("https://x", Some("")).key(), None);
        assert_eq!(cfg("https://x", Some("sk-real")).key(), Some("sk-real"));
        assert_eq!(cfg("https://x", Some("$IMAGE_API_KEY")).unresolved_key_name(),
                   Some("IMAGE_API_KEY"));
    }

    #[test]
    fn data_uri_is_a_png() {
        let g = GeneratedImage {
            b64: "AAAB".into(), mime: "image/png".into(), prompt: "x".into(),
            model: None, seed: None, ms: 1,
        };
        assert_eq!(g.data_uri(), "data:image/png;base64,AAAB");
    }
}
