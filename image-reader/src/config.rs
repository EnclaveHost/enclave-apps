//! App configuration: which model volume to serve, its geometry, the chat
//! template, sampling defaults and the optional API key.
//!
//! Defaults come from the embedded assets/app-config.json (pinned next to the
//! model it describes); a deployment overrides any field through the
//! ENCLAVE_CONFIG env var - a JSON object the platform passes from the
//! deployment's on-chain configCid (CID-verified by the enclave before it
//! reaches us). Publish the app once, deploy it per-model/per-key.
//!
//! MULTI-MODEL: the config JSON also carries a `models` CATALOG -
//! `{ "<volume-name>": { <AppConfig field overrides> }, ... }` - describing
//! every model volume this app knows how to serve (read from the raw JSON, not
//! an AppConfig field). An attached volume is servable when the catalog has an
//! entry for it (or it IS the top-level model_volume); its effective AppConfig
//! = top-level config with the entry's fields overlaid and model_volume pinned
//! to the volume name. ENCLAVE_CONFIG's `models` merges INTO the embedded
//! catalog per volume key and per field within an entry, so a deployment adds
//! one model without restating anything.
//!
//! Every model here is a VISION model: this app has no text-only mode to fall
//! back to, so a volume without a projector is a misconfiguration rather than
//! a lesser model, and it is reported as one (see nn::over_budget and /health).

use serde::Deserialize;

pub static APP_CONFIG_JSON: &[u8] = include_bytes!("../assets/app-config.json");

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    /// model name reported by /v1/models and echoed in answers
    pub name: String,
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
    /// layers that actually hold KV cache, for the VRAM estimate: hybrid /
    /// linear-attention models keep KV only in their full-attention layers.
    /// Defaults to n_layers - the classic all-attention transformer, which is
    /// what every VLM worth attaching here has been so far (Qwen3-VL is
    /// DENSE, and its KV cache is the dominant cost of serving it).
    #[serde(default)]
    pub kv_layers: Option<u32>,
    pub vocab: usize,
    pub eos: Vec<u32>,
    /// chat template: "chatml" | "llama3" | "gemma" | "phi3" | "raw"
    pub template: String,
    /// The instruction the model carries into every answer. This is the one
    /// config field that most changes what this app IS: a describer, a
    /// transcriber, an accessibility captioner, a document reader. The default
    /// is written for a caller that will act on the answer - it asks for what
    /// is visible, and for "illegible" instead of a guess.
    pub system_prompt: String,
    /// what a request with images but NO question is taken to be asking. A
    /// bare image with no instruction is out of distribution for a chat-tuned
    /// VLM, so something always has to be asked; this is the something.
    #[serde(default = "default_question")]
    pub default_question: String,
    pub max_prompt_tokens: usize,
    pub default_max_new: usize,
    pub max_new_cap: usize,
    /// Sampling temperature for requests that don't send one. LOW by design
    /// (0.2, against a chat app's 0.7): the answers here are read as
    /// observations about a picture, and a warm sampler on a vision model is
    /// how "the total is 47" becomes "the total is 41" - a confident wrong
    /// number, which is worse than a clumsy sentence.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default = "default_rep_penalty")]
    pub rep_penalty: f32,
    #[serde(default = "default_rep_window")]
    pub rep_window: usize,
    /// DEGENERATION STOP: how many times one identical block of tokens may
    /// repeat back-to-back before the answer is cut off (0 = off; default 4).
    /// A hard stop, not a sampler nudge. VLMs earn this on dense images -
    /// a table or a list of labels is exactly the input that sends a model
    /// into re-emitting one row until the budget is gone.
    #[serde(default = "default_repeat_guard")]
    pub repeat_guard: usize,
    /// when set, every route except `GET /` and `GET /ping` requires
    /// `Authorization: Bearer <api_key>`. Reference a SECRET by name
    /// ("$IMAGE_READER_API_KEY") in a deployment config, never the literal -
    /// app config is published on-chain by CID.
    ///
    /// Set it. The usual caller is a sibling llm-chat deployment reaching this
    /// one over the fleet's network, which means the endpoint is reachable by
    /// anything else that can dial an IPv6 address, and inference is the
    /// expensive kind of open door.
    #[serde(default)]
    pub api_key: Option<String>,
    /// the attached model volume (Tinfoil Modelwrap) holding the weights: the
    /// platform mounts it read-only at /models/<model_volume>. A vision volume
    /// is an ordinary ggml volume plus one extra file, the *mmproj*.gguf
    /// holding the vision encoder and its projector; the host finds it by that
    /// name and loads it lazily, on the first image the deployment is sent.
    pub model_volume: String,
    /// names the gguf in a multi-quant volume. Keep it matched to the host's
    /// MODEL_VOLUMES pick, which decides what actually preloads.
    #[serde(default)]
    pub model_file: Option<String>,
    /// the tokenizer.json. An ABSOLUTE path escapes the volume, for a weights
    /// repo that ships none.
    #[serde(default)]
    pub tokenizer_file: Option<String>,
    /// how many prompt tokens ONE image is BUDGETED at when deciding whether a
    /// request still fits max_prompt_tokens. Only the host knows the true cost
    /// (a dynamic-resolution model prices an image by its grid, and M-RoPE
    /// numbers positions differently again), so this is a deliberate
    /// over-estimate used for admission control; the real figure comes back
    /// from the host per image and is reported in every answer.
    #[serde(default = "default_image_tokens")]
    pub image_tokens: usize,
    /// hard cap on ONE image, bytes. The UI resizes to ~1.15 MP before upload,
    /// which lands well under 1 MB; this is the guard against a caller that
    /// posts the phone's original.
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: usize,
    /// how many images ONE request may carry. More than one is a real use
    /// (before/after, a spec beside a screenshot), and each costs a
    /// vision-encoder pass plus its own slice of the KV window.
    #[serde(default = "default_max_images")]
    pub max_images: usize,
    /// refuse a request that carries no image at all (the default). This app
    /// exists to look at pictures; a caller that sends none has almost
    /// certainly lost its attachment somewhere in its own plumbing, and
    /// answering from the text alone would hand it a confident-sounding
    /// hallucination instead of the bug report it needs. Set false to allow
    /// text-only turns (follow-up questions with no re-attachment).
    #[serde(default = "default_true")]
    pub require_image: bool,
}

fn default_question() -> String {
    "Describe this image in detail. Transcribe any text exactly as it appears."
        .into()
}
fn default_temperature() -> f32 {
    0.2
}
fn default_top_p() -> f32 {
    0.9
}
fn default_top_k() -> usize {
    40
}
fn default_rep_penalty() -> f32 {
    1.05
}
fn default_rep_window() -> usize {
    64
}
fn default_repeat_guard() -> usize {
    4
}
fn default_image_tokens() -> usize {
    1600
}
fn default_max_image_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_images() -> usize {
    4
}
fn default_true() -> bool {
    true
}

/// The merged config JSON - embedded defaults overlaid with ENCLAVE_CONFIG (if
/// present and valid) - BEFORE a model is chosen. lib.rs keeps the raw value to
/// read the `models` catalog and resolve per-volume entries; a malformed
/// ENCLAVE_CONFIG is reported so a bad deployment config fails loudly instead
/// of silently serving the wrong model shape.
pub fn load_raw() -> Result<serde_json::Value, String> {
    let base: serde_json::Value = serde_json::from_slice(APP_CONFIG_JSON)
        .map_err(|e| format!("embedded app-config.json: {e}"))?;
    match std::env::var("ENCLAVE_CONFIG") {
        Ok(raw) if !raw.trim().is_empty() => {
            let over: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| format!("ENCLAVE_CONFIG is not valid JSON: {e}"))?;
            Ok(merge(base, over))
        }
        _ => Ok(base),
    }
}

pub fn from_value(v: serde_json::Value) -> Result<AppConfig, String> {
    let mut v = v;
    if let Some(o) = v.as_object_mut() {
        o.remove("models");
    }
    serde_json::from_value(v).map_err(|e| format!("config: {e}"))
}

/// The effective AppConfig for one catalog model: `entry`'s fields overlaid on
/// the top-level config, model_volume pinned to the volume the entry is keyed
/// by.
pub fn resolve_entry(
    raw: &serde_json::Value,
    volume: &str,
    entry: serde_json::Value,
) -> Result<AppConfig, String> {
    let mut merged = merge(raw.clone(), entry);
    if let Some(o) = merged.as_object_mut() {
        o.insert("model_volume".into(), serde_json::Value::String(volume.into()));
    }
    from_value(merged)
}

/// Shallow key-wise overlay, except `models`: the catalog merges per volume
/// key, and each entry's fields merge shallowly, so an override can add one
/// model (or tweak one field of a known entry) without restating the rest.
fn merge(mut base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            if k == "models" {
                if let (Some(bm), Some(om)) =
                    (b.get_mut("models").and_then(|m| m.as_object_mut()), v.as_object())
                {
                    for (vol, entry) in om {
                        match (bm.get_mut(vol).and_then(|e| e.as_object_mut()), entry.as_object()) {
                            (Some(be), Some(oe)) => {
                                for (ek, ev) in oe {
                                    be.insert(ek.clone(), ev.clone());
                                }
                            }
                            _ => {
                                bm.insert(vol.clone(), entry.clone());
                            }
                        }
                    }
                    continue;
                }
            }
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

/// Where an image sits inside a turn's text. The template renders the whole
/// request as ONE string, so an attachment leaves this mark behind and the
/// prompt is split on it afterwards into (text, image, text) runs; the text
/// runs tokenize normally and the image goes to the host as file bytes.
///
/// A private-use codepoint pair: no tokenizer vocabulary contains it, no
/// keyboard produces it, and incoming text is stripped of it before rendering
/// so a crafted question cannot forge an image slot it did not attach.
pub const MEDIA_MARK: &str = "\u{E000}\u{E001}";

/// A rendered prompt plus the strings that should terminate generation for
/// this template (in addition to the tokenizer-level EOS ids).
pub struct Rendered {
    pub prompt: String,
    pub stop_strings: Vec<String>,
}

/// Render the conversation in the model's own chat format. Deliberately the
/// same four templates (plus "raw") the sibling llm-chat app renders, and
/// deliberately NO think-turn handling: a VLM that reasons before answering
/// is welcome to, but this app never force-opens a `<think>` block, because
/// its answers are consumed programmatically as often as they are read.
pub fn render_template(
    template: &str,
    system: &str,
    msgs: &[(String, String)], // (role, content), roles pre-filtered to user/assistant
) -> Result<Rendered, String> {
    let mut p = String::new();
    let stops: Vec<String>;
    match template {
        "chatml" => {
            p.push_str(&format!("<|im_start|>system\n{system}<|im_end|>\n"));
            for (role, content) in msgs {
                p.push_str(&format!("<|im_start|>{role}\n{content}<|im_end|>\n"));
            }
            p.push_str("<|im_start|>assistant\n");
            stops = vec!["<|im_end|>".into(), "<|im_start|>".into()];
        }
        "llama3" => {
            p.push_str(&format!(
                "<|begin_of_text|><|start_header_id|>system<|end_header_id|>\n\n{system}<|eot_id|>"
            ));
            for (role, content) in msgs {
                p.push_str(&format!(
                    "<|start_header_id|>{role}<|end_header_id|>\n\n{content}<|eot_id|>"
                ));
            }
            p.push_str("<|start_header_id|>assistant<|end_header_id|>\n\n");
            stops = vec!["<|eot_id|>".into()];
        }
        "gemma" => {
            // gemma has no system role; fold it into the first user turn
            let mut first = true;
            for (role, content) in msgs {
                let r = if role == "assistant" { "model" } else { "user" };
                let c = if first && r == "user" && !system.is_empty() {
                    first = false;
                    format!("{system}\n\n{content}")
                } else {
                    first = false;
                    content.clone()
                };
                p.push_str(&format!("<start_of_turn>{r}\n{c}<end_of_turn>\n"));
            }
            p.push_str("<start_of_turn>model\n");
            stops = vec!["<end_of_turn>".into()];
        }
        "phi3" => {
            p.push_str(&format!("<|system|>\n{system}<|end|>\n"));
            for (role, content) in msgs {
                p.push_str(&format!("<|{role}|>\n{content}<|end|>\n"));
            }
            p.push_str("<|assistant|>\n");
            stops = vec!["<|end|>".into()];
        }
        "raw" => {
            if !system.is_empty() {
                p.push_str(system);
                p.push_str("\n\n");
            }
            for (_, content) in msgs {
                p.push_str(content);
                p.push('\n');
            }
            stops = vec![];
        }
        other => {
            return Err(format!("unknown template '{other}' (chatml|llama3|gemma|phi3|raw)"))
        }
    }
    Ok(Rendered { prompt: p, stop_strings: stops })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclave_config_merges_per_field_and_per_model() {
        let base = serde_json::json!({
            "name": "a", "temperature": 0.2,
            "models": { "vol-a": { "name": "a", "image_tokens": 1600 } }
        });
        let over = serde_json::json!({
            "temperature": 0.5,
            "models": { "vol-a": { "image_tokens": 900 }, "vol-b": { "name": "b" } }
        });
        let m = merge(base, over);
        assert_eq!(m["temperature"], 0.5);
        // the untouched field of a known entry survives
        assert_eq!(m["models"]["vol-a"]["name"], "a");
        assert_eq!(m["models"]["vol-a"]["image_tokens"], 900);
        assert_eq!(m["models"]["vol-b"]["name"], "b");
    }

    #[test]
    fn the_catalog_never_reaches_serde() {
        // a stray "models" key would be an unknown field for AppConfig; both
        // entry points strip it, so a config with a catalog parses
        let raw = serde_json::json!({
            "name": "x", "n_layers": 1, "n_kv_heads": 1, "head_dim": 1, "vocab": 2,
            "eos": [0], "template": "chatml", "system_prompt": "s",
            "max_prompt_tokens": 10, "default_max_new": 1, "max_new_cap": 1,
            "model_volume": "v", "models": { "v": {} }
        });
        assert!(from_value(raw.clone()).is_ok());
        let cfg = resolve_entry(&raw, "other", serde_json::json!({ "name": "y" })).unwrap();
        assert_eq!(cfg.name, "y");
        assert_eq!(cfg.model_volume, "other");
    }

    #[test]
    fn embedded_config_parses_and_every_catalog_entry_resolves() {
        let raw: serde_json::Value = serde_json::from_slice(APP_CONFIG_JSON).unwrap();
        from_value(raw.clone()).expect("top-level config");
        for (vol, entry) in raw["models"].as_object().unwrap() {
            let cfg = resolve_entry(&raw, vol, entry.clone())
                .unwrap_or_else(|e| panic!("catalog entry {vol}: {e}"));
            assert_eq!(&cfg.model_volume, vol);
            // a template this app cannot render is a config bug, not a
            // runtime surprise
            render_template(&cfg.template, "s", &[("user".into(), "hi".into())]).unwrap();
        }
    }

    #[test]
    fn chatml_opens_an_assistant_turn_without_a_think_block() {
        let r = render_template("chatml", "sys", &[("user".into(), "q".into())]).unwrap();
        assert!(r.prompt.ends_with("<|im_start|>assistant\n"));
        assert!(!r.prompt.contains("<think>"));
        assert!(r.stop_strings.contains(&"<|im_end|>".to_string()));
    }
}
