//! App configuration: model geometry, chat template, sampling defaults and
//! the optional API key. Defaults come from the embedded assets/app-config.json
//! (pinned next to the model it describes); a deployment can override any
//! field through the ENCLAVE_CONFIG env var - a JSON object the platform passes
//! from the deployment's on-chain configCid (CID-verified by the enclave
//! before it reaches us). Publish the app once, deploy it per-model/per-key.
//!
//! MULTI-MODEL: the config JSON additionally carries a `models` CATALOG -
//! `{ "<volume-name>": { <AppConfig field overrides> }, ... }` - describing
//! every model volume this app knows how to serve (it is read from the raw
//! JSON, not an AppConfig field). An attached volume is servable when the
//! catalog has an entry for it (or it IS the top-level model_volume); its
//! effective AppConfig = top-level config with the entry's fields overlaid
//! and model_volume pinned to the volume name. The embedded catalog ships
//! the known models; ENCLAVE_CONFIG's `models` merges INTO it per volume key
//! (and per field within an entry), so a deployment adds one model - e.g.
//! {"models":{"qwen3.5-122b-a10b":{"name":"qwen3.5-122b","backend":"ggml"}}}
//! - without restating anything. Which model serves a given request is
//! decided in lib.rs (largest attached by weights size, unless the request
//! names one).

use serde::Deserialize;

pub static APP_CONFIG_JSON: &[u8] = include_bytes!("../assets/app-config.json");

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    /// model name reported by /v1/models and echoed in completions
    pub name: String,
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
    /// layers that actually hold KV cache, for VRAM estimates: hybrid /
    /// linear-attention models keep KV only in their full-attention layers
    /// (qwen3.5-9b: 8 of 32; qwen3.5-122b: 12 of 48). Defaults to n_layers -
    /// the classic all-attention transformer.
    #[serde(default)]
    pub kv_layers: Option<u32>,
    pub vocab: usize,
    pub eos: Vec<u32>,
    /// chat template: "chatml" | "llama3" | "gemma" | "phi3" | "raw"
    pub template: String,
    pub system_prompt: String,
    pub max_prompt_tokens: usize,
    pub default_max_new: usize,
    pub max_new_cap: usize,
    pub rep_penalty: f32,
    pub rep_window: usize,
    /// when set, /v1/* requires `Authorization: Bearer <api_key>`. The chat
    /// UI and legacy /chat stay open - gate those with a PRIVATE deployment.
    #[serde(default)]
    pub api_key: Option<String>,
    /// inference backend: "onnx" (default - model bytes read from the volume,
    /// KV cache shuttled through wasi-nn tensors, model must fit guest memory)
    /// or "ggml" (a GGUF the HOST preloaded from the same volume; weights
    /// never enter guest memory, so model size is bounded by the deployment's
    /// share - this is the big-model path).
    #[serde(default = "default_backend")]
    pub backend: String,
    /// the attached model volume (Tinfoil Modelwrap) holding the weights: the
    /// platform mounts it read-only at /models/<model_volume>. Optional file
    /// overrides for non-standard layouts, tried BEFORE the conventional
    /// candidates (onnx/model_q4.onnx, model_q4.onnx, onnx/model.onnx,
    /// model.onnx; tokenizer.json). An ABSOLUTE path escapes the volume
    /// (e.g. tokenizer_file "/models/qwen2.5-0.5b/tokenizer.json" when the
    /// weights repo ships no tokenizer). For ggml, model_file names the gguf
    /// in a multi-quant volume - keep it matched to the host's MODEL_VOLUMES
    /// pick, which decides what actually preloads. A split GGUF
    /// ("<prefix>-NNNNN-of-MMMMM.gguf", any part) counts as the whole family.
    pub model_volume: String,
    #[serde(default)]
    pub model_file: Option<String>,
    #[serde(default)]
    pub tokenizer_file: Option<String>,
}

fn default_backend() -> String {
    "onnx".into()
}

/// The merged config JSON - embedded defaults overlaid with ENCLAVE_CONFIG
/// (if present and valid) - BEFORE a model is chosen. lib.rs keeps the raw
/// value to read the `models` catalog and resolve per-volume entries; a
/// malformed ENCLAVE_CONFIG is reported so a bad deployment config fails
/// loudly instead of silently serving the wrong model shape. Unknown fields
/// are ignored at parse time (from_value).
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
    serde_json::from_value(v).map_err(|e| format!("config: {e}"))
}

/// The effective AppConfig for one catalog model: `entry`'s fields overlaid
/// on the top-level config, model_volume pinned to the volume the entry is
/// keyed by. The catalog itself is dropped from the result so a stray
/// "models" key can never reach serde.
pub fn resolve_entry(
    raw: &serde_json::Value,
    volume: &str,
    entry: serde_json::Value,
) -> Result<AppConfig, String> {
    let mut merged = merge(raw.clone(), entry);
    if let Some(o) = merged.as_object_mut() {
        o.remove("models");
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
                if let (Some(bm), Some(om)) = (
                    b.get_mut("models").and_then(|m| m.as_object_mut()),
                    v.as_object(),
                ) {
                    for (vol, entry) in om {
                        match (
                            bm.get_mut(vol).and_then(|e| e.as_object_mut()),
                            entry.as_object(),
                        ) {
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

/// A rendered prompt plus the strings that should terminate generation for
/// this template (in addition to the tokenizer-level EOS ids).
pub struct Rendered {
    pub prompt: String,
    pub stop_strings: Vec<String>,
}

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
            // plain concatenation for base models: no roles, no control tokens
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
        other => return Err(format!("unknown template '{other}' (chatml|llama3|gemma|phi3|raw)")),
    }
    Ok(Rendered { prompt: p, stop_strings: stops })
}
