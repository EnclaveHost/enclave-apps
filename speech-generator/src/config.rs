//! App configuration: which model volume to speak with, its geometry, the
//! voice presets, sampling defaults and the optional API key.
//!
//! Defaults come from the embedded assets/app-config.json (pinned next to the
//! model it describes); a deployment overrides any field through the
//! ENCLAVE_CONFIG env var - a JSON object the platform passes from the
//! deployment's on-chain configCid (CID-verified by the enclave before it
//! reaches us). Publish the app once, deploy it per-model/per-key.
//!
//! MULTI-MODEL: the config JSON also carries a `models` CATALOG -
//! `{ "<volume-name>": { <AppConfig field overrides> }, ... }` - describing
//! every model volume this app knows how to serve, exactly as in the sibling
//! image-reader app. An attached volume is servable when the catalog has an
//! entry for it (or it IS the top-level model_volume). ENCLAVE_CONFIG's
//! `models` merges INTO the embedded catalog per volume key and per field.
//!
//! Every model here is a SNAC-token speech model (Maya1 today): a Llama-style
//! decoder whose "vocabulary" ends in 28,672 audio-codec tokens. A plain LLM
//! volume attached to this app would sample garbage, which is why the catalog
//! gates serving the same way image-reader's does.

use serde::Deserialize;
use std::collections::BTreeMap;

pub static APP_CONFIG_JSON: &[u8] = include_bytes!("../assets/app-config.json");

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    /// model name reported by /v1/models and echoed in answers
    pub name: String,
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
    /// layers that actually hold KV cache, for the VRAM estimate (see
    /// image-reader). Maya1 is a dense Llama - all 28 layers.
    #[serde(default)]
    pub kv_layers: Option<u32>,
    /// FULL vocabulary including the audio-token range - the guest asserts the
    /// host's logits row is exactly this long, so a mismatched volume fails in
    /// a sentence instead of sampling from the wrong id space.
    pub vocab: usize,
    /// voice presets: name -> a natural-language voice description in the form
    /// Maya1 was trained on ("Female voice in her 30s, warm, American accent,
    /// conversational pace"). A request may name one of these or bring its own
    /// description; `default_voice` must be a key of this map.
    pub voices: BTreeMap<String, String>,
    pub default_voice: String,
    /// Sampling defaults, straight from the Maya1 model card: temperature 0.4,
    /// top_p 0.9, repetition penalty 1.1. LOW temperature by design - an audio
    /// token sampled badly is a click or a warble, not a clumsy word.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_rep_penalty")]
    pub rep_penalty: f32,
    /// repetition-penalty window over the generated audio tokens; 0 = the
    /// whole generation (the reference vLLM behaviour).
    #[serde(default)]
    pub rep_window: usize,
    /// longest text one request may carry, in characters (after whitespace
    /// normalisation). The lever that bounds one request's GPU time.
    #[serde(default = "default_max_text_chars")]
    pub max_text_chars: usize,
    /// text is spoken in chunks of at most this many characters, split at
    /// sentence boundaries; each chunk is one generation episode (fresh
    /// context), which is how every SNAC-LLM pipeline does long-form.
    #[serde(default = "default_chunk_max_chars")]
    pub chunk_max_chars: usize,
    /// audio-token budget for ONE chunk's generation (2048 = ~25 s of audio,
    /// the model card's max). The chunker keeps chunks well under this; the
    /// cap is the backstop against a generation that will not stop.
    #[serde(default = "default_max_new_tokens")]
    pub max_new_tokens: usize,
    /// the model card's minimum before EOS is allowed (28 tokens = 4 frames).
    #[serde(default = "default_min_frames")]
    pub min_frames: usize,
    /// samples trimmed from the head of each chunk's audio (the model card's
    /// warmup trim: 2048 samples = exactly one SNAC frame).
    #[serde(default = "default_trim_warmup")]
    pub trim_warmup_samples: usize,
    /// when set, the /v1/* surface requires `Authorization: Bearer <api_key>`.
    /// Reference a SECRET by name ("$SPEECH_GENERATOR_API_KEY") in a deployment
    /// config, never the literal - app config is published on-chain by CID.
    #[serde(default)]
    pub api_key: Option<String>,
    /// the attached model volume (Tinfoil Modelwrap) holding the weights: the
    /// platform mounts it read-only at /models/<model_volume>. A speech volume
    /// is an ordinary ggml volume plus TWO extra files - tokenizer.json (guest
    /// tokenizes the prompt) and snac_decoder.bin (the guest-side codec).
    pub model_volume: String,
    /// names the gguf in a multi-quant volume; keep matched to the host's pick.
    #[serde(default)]
    pub model_file: Option<String>,
    /// the tokenizer.json; ABSOLUTE to escape the volume (see image-reader).
    #[serde(default)]
    pub tokenizer_file: Option<String>,
    /// the SNAC decoder weights within the volume (or absolute).
    #[serde(default = "default_snac_file")]
    pub snac_file: String,
}

fn default_temperature() -> f32 {
    0.4
}
fn default_top_p() -> f32 {
    0.9
}
fn default_rep_penalty() -> f32 {
    1.1
}
fn default_max_text_chars() -> usize {
    4000
}
fn default_chunk_max_chars() -> usize {
    600
}
fn default_max_new_tokens() -> usize {
    2048
}
fn default_min_frames() -> usize {
    4
}
fn default_trim_warmup() -> usize {
    2048
}
fn default_snac_file() -> String {
    "snac_decoder.bin".into()
}

impl AppConfig {
    /// The voice description one request should speak with. An explicit
    /// `description` wins; a `voice` naming a preset resolves through the
    /// table; any other non-empty `voice` string IS a description (that rule
    /// is what lets an OpenAI-SDK caller put the whole voice brief in the
    /// `voice` field, which is the only voice-shaped field the SDK has); and
    /// nothing at all means the configured default.
    pub fn resolve_voice(&self, voice: Option<&str>, description: Option<&str>) -> String {
        if let Some(d) = description.map(str::trim).filter(|d| !d.is_empty()) {
            return d.to_string();
        }
        if let Some(v) = voice.map(str::trim).filter(|v| !v.is_empty()) {
            if let Some(desc) = self.voices.get(v) {
                return desc.clone();
            }
            return v.to_string();
        }
        self.voices
            .get(&self.default_voice)
            .cloned()
            .unwrap_or_else(|| "Female voice in her early 30s, warm and clear, American accent, conversational pace".into())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclave_config_merges_per_field_and_per_model() {
        let base = serde_json::json!({
            "name": "a", "temperature": 0.4,
            "models": { "vol-a": { "name": "a", "max_new_tokens": 2048 } }
        });
        let over = serde_json::json!({
            "temperature": 0.5,
            "models": { "vol-a": { "max_new_tokens": 4096 }, "vol-b": { "name": "b" } }
        });
        let m = merge(base, over);
        assert_eq!(m["temperature"], 0.5);
        assert_eq!(m["models"]["vol-a"]["name"], "a");
        assert_eq!(m["models"]["vol-a"]["max_new_tokens"], 4096);
        assert_eq!(m["models"]["vol-b"]["name"], "b");
    }

    #[test]
    fn embedded_config_parses_and_every_catalog_entry_resolves() {
        let raw: serde_json::Value = serde_json::from_slice(APP_CONFIG_JSON).unwrap();
        let cfg = from_value(raw.clone()).expect("top-level config");
        // the default voice must resolve, or every bare request 500s
        assert!(cfg.voices.contains_key(&cfg.default_voice));
        for (vol, entry) in raw["models"].as_object().unwrap() {
            let cfg = resolve_entry(&raw, vol, entry.clone())
                .unwrap_or_else(|e| panic!("catalog entry {vol}: {e}"));
            assert_eq!(&cfg.model_volume, vol);
            assert!(cfg.voices.contains_key(&cfg.default_voice));
        }
    }

    #[test]
    fn voice_resolution_prefers_description_then_preset_then_literal() {
        let raw: serde_json::Value = serde_json::from_slice(APP_CONFIG_JSON).unwrap();
        let cfg = from_value(raw).unwrap();
        let preset = cfg.voices[&cfg.default_voice].clone();
        // nothing at all -> the default preset
        assert_eq!(cfg.resolve_voice(None, None), preset);
        // a preset name resolves through the table
        let (name, desc) = cfg.voices.iter().next().unwrap();
        assert_eq!(cfg.resolve_voice(Some(name), None), *desc);
        // an unknown voice string IS the description
        assert_eq!(
            cfg.resolve_voice(Some("Robot voice, monotone, fast"), None),
            "Robot voice, monotone, fast"
        );
        // an explicit description beats everything
        assert_eq!(cfg.resolve_voice(Some(name), Some("Whispering elf")), "Whispering elf");
        // whitespace-only fields do not shadow the default
        assert_eq!(cfg.resolve_voice(Some("  "), Some("")), preset);
    }
}
