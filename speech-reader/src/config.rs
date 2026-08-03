//! App configuration: which model volume transcribes, its geometry, the
//! instruction wording, admission limits and the optional API key.
//!
//! Defaults come from the embedded assets/app-config.json; a deployment
//! overrides any field through ENCLAVE_CONFIG (the on-chain configCid,
//! CID-verified by the enclave), and the `models` CATALOG merges per volume
//! key and per field - the same layering as the sibling GPU apps.
//!
//! Every model here is a SPEECH model: an audio encoder (the volume's
//! *mmproj*.gguf) projecting into an LM fine-tuned to transcribe. The prompt
//! is not a chat - Granite Speech ships a bare "USER: ... ASSISTANT:"
//! template - so instead of a template engine this config carries the exact
//! prefix/suffix strings around the (audio, instruction) pair, which is both
//! simpler and honest about how single-turn these models are.

use serde::Deserialize;

pub static APP_CONFIG_JSON: &[u8] = include_bytes!("../assets/app-config.json");

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    /// model name reported by /v1/models and echoed in answers
    pub name: String,
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
    /// layers that hold KV, for the VRAM estimate (granite-speech-4.1's LM is
    /// a DENSE granite - all 40 layers)
    #[serde(default)]
    pub kv_layers: Option<u32>,
    pub vocab: usize,
    pub eos: Vec<u32>,
    /// the transcription instruction, placed AFTER the audio in the turn -
    /// the wording the model was trained on. Changing it changes the task:
    /// "transcribe the speech with proper punctuation and capitalization."
    pub instruction: String,
    /// what /v1/audio/translations asks for (Granite Speech does bidirectional
    /// AST to/from English; this app exposes the to-English direction OpenAI
    /// gave that endpoint)
    #[serde(default = "default_translate")]
    pub translate_instruction: String,
    /// the turn's frame: prefix, then the audio, then the instruction, then
    /// suffix. Granite Speech 4.1's shipped chat template is exactly
    /// "USER: {content}\n ASSISTANT:" - no role tokens, no BOS.
    #[serde(default = "default_prefix")]
    pub prompt_prefix: String,
    #[serde(default = "default_suffix")]
    pub prompt_suffix: String,
    /// strings that end generation beyond the tokenizer-level eos ids - the
    /// guard against a model that starts inventing its next turn
    #[serde(default = "default_stops")]
    pub stop_strings: Vec<String>,
    /// admission-control estimate of the positions ONE second of audio costs
    /// after the encoder+projector downsample (~10/s for granite-speech;
    /// deliberately over-estimated - the true figure comes back from the host
    /// per request and is reported in every answer)
    #[serde(default = "default_audio_tokens_per_second")]
    pub audio_tokens_per_second: usize,
    /// the position budget one episode must fit: audio positions + prompt +
    /// transcript. Granite Speech 4.1's LM is trained at 4096 positions
    /// (config.json max_position_embeddings), so the default leaves headroom
    /// under that, NOT under the node's (much larger) allocated window.
    #[serde(default = "default_max_positions")]
    pub max_positions: usize,
    /// WAV inputs longer than this are split at quiet points into separate
    /// transcription episodes. Sized so audio positions + transcript fit
    /// max_positions with room: 240 s ~ 2900 positions + ~1000 transcript.
    #[serde(default = "default_chunk_seconds")]
    pub chunk_seconds: usize,
    /// hard cap on ONE request's audio duration (WAV, where duration is
    /// knowable up front). An hour of audio is ~15 episodes and minutes of
    /// GPU time - a bigger job than one HTTP request should be.
    #[serde(default = "default_max_audio_seconds")]
    pub max_audio_seconds: usize,
    /// hard cap on the audio file's bytes - the only admission control
    /// available for compressed formats (mp3/flac), whose duration is not
    /// worth parsing out here.
    #[serde(default = "default_max_audio_bytes")]
    pub max_audio_bytes: usize,
    pub default_max_new: usize,
    pub max_new_cap: usize,
    /// 0.0 = greedy, the right default for transcription: sampling noise on
    /// an ASR model does not make it more right, only differently wrong.
    #[serde(default)]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default)]
    pub top_k: usize,
    /// repetition penalty default OFF (1.0): real speech repeats itself, and
    /// penalizing that turns "no, no, no" into an invention
    #[serde(default = "default_rep_penalty")]
    pub rep_penalty: f32,
    #[serde(default = "default_rep_window")]
    pub rep_window: usize,
    /// identical consecutive token blocks that end a degenerate transcript
    /// (0 = off). Silence, hold music and hum are exactly the inputs that
    /// send an ASR decoder into a loop.
    #[serde(default = "default_repeat_guard")]
    pub repeat_guard: usize,
    /// when set, the /v1/* surface requires `Authorization: Bearer <api_key>`.
    /// Reference a SECRET by name ("$SPEECH_READER_API_KEY"), never the
    /// literal - app config is published on-chain by CID.
    #[serde(default)]
    pub api_key: Option<String>,
    /// the attached model volume: model gguf + *mmproj*.gguf (the conformer
    /// audio encoder + projector, found by name exactly as a vision volume's
    /// projector is) + tokenizer.json
    pub model_volume: String,
    #[serde(default)]
    pub model_file: Option<String>,
    #[serde(default)]
    pub tokenizer_file: Option<String>,
}

fn default_translate() -> String {
    "translate the speech to English.".into()
}
fn default_prefix() -> String {
    "USER: ".into()
}
fn default_suffix() -> String {
    "\n ASSISTANT:".into()
}
fn default_stops() -> Vec<String> {
    vec!["USER:".into()]
}
fn default_audio_tokens_per_second() -> usize {
    12
}
fn default_max_positions() -> usize {
    4000
}
fn default_chunk_seconds() -> usize {
    240
}
fn default_max_audio_seconds() -> usize {
    7200
}
fn default_max_audio_bytes() -> usize {
    32 * 1024 * 1024
}
fn default_top_p() -> f32 {
    1.0
}
fn default_rep_penalty() -> f32 {
    1.0
}
fn default_rep_window() -> usize {
    64
}
fn default_repeat_guard() -> usize {
    4
}

/// The merged config JSON (embedded defaults + ENCLAVE_CONFIG), BEFORE a
/// model is chosen; the raw value keeps the `models` catalog.
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

/// The effective AppConfig for one catalog model, model_volume pinned.
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

/// Shallow key-wise overlay, except `models`: per volume key, per field.
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
    fn embedded_config_parses_and_every_catalog_entry_resolves() {
        let raw: serde_json::Value = serde_json::from_slice(APP_CONFIG_JSON).unwrap();
        let cfg = from_value(raw.clone()).expect("top-level config");
        // the frame must hold the audio slot between prefix and suffix
        assert!(cfg.prompt_suffix.contains("ASSISTANT"));
        // an episode must be able to hold a chunk plus its transcript
        assert!(
            cfg.chunk_seconds * cfg.audio_tokens_per_second + cfg.default_max_new / 2
                < cfg.max_positions,
            "chunk_seconds x audio_tokens_per_second leaves no room to answer"
        );
        for (vol, entry) in raw["models"].as_object().unwrap() {
            let cfg = resolve_entry(&raw, vol, entry.clone())
                .unwrap_or_else(|e| panic!("catalog entry {vol}: {e}"));
            assert_eq!(&cfg.model_volume, vol);
        }
    }

    #[test]
    fn enclave_config_merges_per_field_and_per_model() {
        let base = serde_json::json!({
            "name": "a", "temperature": 0.0,
            "models": { "vol-a": { "name": "a", "chunk_seconds": 240 } }
        });
        let over = serde_json::json!({
            "temperature": 0.2,
            "models": { "vol-a": { "chunk_seconds": 120 }, "vol-b": { "name": "b" } }
        });
        let m = merge(base, over);
        assert_eq!(m["temperature"], 0.2);
        assert_eq!(m["models"]["vol-a"]["name"], "a");
        assert_eq!(m["models"]["vol-a"]["chunk_seconds"], 120);
        assert_eq!(m["models"]["vol-b"]["name"], "b");
    }
}
