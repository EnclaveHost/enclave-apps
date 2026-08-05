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
    /// this model reasons in <think> blocks before answering (qwen3.x).
    /// The qwen3.x templates FORCE-OPEN the block: `<think>\n` belongs to
    /// the prompt's assistant turn and the model only generates the
    /// reasoning + `</think>` - it never emits the opening tag itself (a
    /// bare assistant turn is out-of-distribution and collapses to the
    /// empty no-think pair, skipping reasoning entirely). Thinking on =
    /// prefill `<think>\n` (chatml only) and re-emit the tag in the output
    /// so clients see a complete block; a request's enable_thinking=false
    /// swaps in the trained no-think form, a pre-closed empty block. Off
    /// (the default) means the switch is ignored - pre-filling think
    /// tokens a model never trained on degrades it.
    #[serde(default)]
    pub thinking: bool,
    /// how many tokens the reply may spend INSIDE its <think> block before
    /// the block is force-closed and the answer has to start (0 = uncapped,
    /// the default). Reasoning models can get stuck in the block, re-deriving
    /// the same step until max_new is gone, and the user is left with a wall
    /// of reasoning and no answer. At the budget the server appends
    /// `\n</think>\n\n` to the reply AND feeds it to the model, which then
    /// writes its answer from the reasoning it already has. Only ever armed
    /// on a turn whose prompt force-opened the block (a `thinking` model with
    /// thinking left on), so it is inert everywhere else. Counts toward the
    /// same max_new budget as the rest of the reply: leave enough headroom
    /// under default_max_new for an answer.
    #[serde(default)]
    pub think_budget: usize,
    /// EFFORT SCALING: size the reasoning budget to the QUESTION instead of
    /// giving every turn the same ceiling. Absent (the default) means
    /// think_budget applies flat, exactly as before.
    ///
    /// Present, a one-line classifier pass rates the turn low / medium / high
    /// and the budget for THAT turn comes from the matching field below. It is
    /// the same pass the web-search router already runs when auto search is on,
    /// so a deployment with both pays for one classification, not two; with
    /// search off it is one extra short greedy generation per turn.
    ///
    /// Read what this does and does not buy: think_budget is a CEILING on a
    /// runaway, not a target. A model that reasons for 200 tokens and answers
    /// is untouched by any of these numbers. What scaling changes is how long a
    /// model that will NOT stop gets to run before the block is closed for it -
    /// seconds on a greeting instead of the better part of a minute.
    #[serde(default)]
    pub effort: Option<EffortConfig>,
    pub system_prompt: String,
    pub max_prompt_tokens: usize,
    pub default_max_new: usize,
    pub max_new_cap: usize,
    /// sampling temperature for requests that don't send one (0 = greedy,
    /// clamped 0.0..=2.0 like the request field). Per-model, so a deployment
    /// can pin the value its model was tuned for instead of relying on every
    /// client to ask for it: the built-in chat UI sends no sampling params at
    /// all, so this is what it gets.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// nucleus sampling for requests that don't send one: the smallest set of
    /// candidates whose probabilities sum to top_p (1.0 = off).
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    /// top-k truncation for requests that don't send one: consider only the k
    /// likeliest tokens (0 = off, which is the historical default).
    ///
    /// Worth setting on a reasoning model. With top_k off the candidate set is
    /// whatever the nucleus admits out of a 256-wide prefilter, and that tail
    /// is where a reply that has lost the thread goes wandering: measured on
    /// the fable 27b (2026-07-28, "write a haiku", 1200 tokens), repeated
    /// 10-grams ran 36.3% at top_k 0 and 10.6% at top_k 40, while raising
    /// temperature alone barely moved it (31.8% at 0.9). Truncation, not
    /// temperature, is the lever against a degenerate reply.
    #[serde(default)]
    pub top_k: usize,
    pub rep_penalty: f32,
    pub rep_window: usize,
    /// DEGENERATION STOP: how many times one identical block of tokens may
    /// repeat back-to-back before the reply is cut off (0 = off; default 4).
    ///
    /// Unlike rep_penalty and top_k this is not a nudge to the sampler, it is
    /// a hard stop, and unlike think_budget it applies to the WHOLE reply
    /// rather than only inside a force-opened `<think>` block. That gap is
    /// what let a looping haiku run to 80k tokens: the model was reasoning in
    /// plain prose, so the think budget never armed and rep_penalty 1 meant
    /// nothing was pushing back. Leave it on; it costs a tail comparison per
    /// token and only ever fires on text no user wanted.
    #[serde(default = "default_repeat_guard")]
    pub repeat_guard: usize,
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
    /// "local" (default) parses tokenizer.json in the guest; "host" uses the
    /// GGUF's own tokenizer through the session verbs (encode + piece table),
    /// skipping the multi-MB parse every fresh instance pays - falls back to
    /// local on hosts that predate the verbs.
    #[serde(default = "default_tokenizer")]
    pub tokenizer: String,
    /// SPECULATIVE DECODING: what proposes tokens for this model to verify.
    /// The special value "mtp" uses the model's OWN trained multi-token-
    /// prediction head (an *-MTP GGUF variant; near-zero proposal cost, no
    /// second model, the preferred setting when the volume carries a head).
    /// "lookup" = prompt-lookup drafting: n-gram matches against the
    /// conversation itself propose what followed last time - zero proposal
    /// cost, works on ANY ggml model, and rounds without a match decode
    /// plain, so it never costs baseline speed. Shines on repetition-heavy
    /// output (code, quoting, lists); modest on freeform prose.
    /// Any other value names a catalog model (by volume or name) - a small
    /// same-tokenizer sibling that proposes `draft_tokens` ahead. Either
    /// way the target verifies proposals in ONE pass and every accepted
    /// token skips a full target step; output is byte-for-byte the
    /// target's own. A model draft requires the ggml backend on both AND an
    /// identical tokenizer (same vocab, byte-equal tokenizer.json) - a
    /// mismatched pair is REFUSED at request time. All draft-side failures
    /// (no head, unattached, unfit, busy, stale host) fall back to plain
    /// decode with a status note. qwen3.x models share one tokenizer;
    /// qwen2.5-0.5b does NOT draft for them.
    #[serde(default)]
    pub draft: Option<String>,
    /// tokens drafted per speculative round (clamped 1..=16; default 8)
    #[serde(default = "default_draft_tokens")]
    pub draft_tokens: usize,
    /// "mtp" drafts only: minimum head confidence to keep proposing
    /// (0.0..=0.95; default 0.4 - lower = longer, riskier drafts). 0 means
    /// ALWAYS draft the full `draft_tokens`: the engine skips the confidence
    /// softmax outright, and constant-length drafts keep every verify pass
    /// the same ubatch shape so the CUDA backend can keep replaying its
    /// captured graph instead of re-warming (the launch-latency win that
    /// makes speculation beat plain decode).
    #[serde(default = "default_draft_p_min")]
    pub draft_p_min: f32,
    /// "lookup" drafts only: shortest n-gram anchor allowed to propose
    /// (clamped 2..=6; absent = the built-in 4). The right floor depends on
    /// `draft_tokens`, because a misfired verify round costs only the
    /// batch-width difference over a plain step: ~12.7 ms at k=4 but ~2.0 ms
    /// at k=1 (measured, 9b, in-app feed_all#decode). Break-even acceptance
    /// is that tax over the plain step cost, so k=4 needs ~33% and k=1 needs
    /// ~11% - anchors that are pure waste at k=4 can pay at k=1.
    #[serde(default)]
    pub draft_min_ngram: Option<usize>,
    /// "lookup" drafts only: set false to disable the acceptance gate (the
    /// EMA pause + anchor escalation). The gate exists because a mediocre
    /// k=4 round is expensive; at k=1 the tax is small enough that pausing
    /// can cost more throughput than the misfires it avoids.
    #[serde(default)]
    pub draft_gate: Option<bool>,
    /// "mtp" drafts only: set false to refuse the engine's fused round verb
    /// ("mtp_round", mm25 hosts) and keep the two-call draft/verify loop.
    /// The fused call is semantically identical - same observe ordering,
    /// same verify batch, same rows - and saves a guest boundary crossing
    /// plus an arbiter grant per round (~2-3 ms of a ~28 ms k=1 round on
    /// the fleet). This knob exists for A/B measurement, not as a mode.
    #[serde(default)]
    pub draft_fused: Option<bool>,
    /// WEB SEARCH: absent (the default) means the app never makes an outbound
    /// request and the UI hides the control entirely - a deployment opts IN.
    /// Present means a request may ask for search, and the app fetches results
    /// itself over wasi:http so the query goes out from the deployment's own
    /// egress identity rather than the user's browser. See search.rs for what
    /// the provider does and does not learn.
    ///
    /// LEGACY LOCATION: since 0.39 the block's home is `tools.search`, beside
    /// the other external capabilities. This top-level spelling still parses
    /// forever (config CIDs are immutable on-chain); when both are present the
    /// tools one wins. Read it through search_cfg(), never directly.
    #[serde(default)]
    pub search: Option<crate::search::SearchConfig>,
    /// IMAGE GENERATION: absent (the default) means the app never calls an
    /// image service and the model is never told it could. Present points at
    /// one - normally the sibling image-generator app's deployment - and the
    /// same router that decides about web search decides about images.
    #[serde(default)]
    pub image: Option<crate::image::ImageConfig>,
    /// TOOLS: HTTP endpoints and MCP servers this deployment lets the model
    /// call mid-answer. Absent (the default) means the model is never told a
    /// tool exists and the app never calls one.
    ///
    /// The registry is the DEPLOYMENT'S, never the client's: a request can turn
    /// the feature off (and by default it is off, exactly like web search) but
    /// it cannot add an entry or change a URL. See tools.rs for why that
    /// boundary is not negotiable.
    ///
    /// Tool calling is a TRAINED format, so this only arms on a `chatml`
    /// (qwen-family) model; elsewhere the block is not rendered and the
    /// pre-pass router keeps doing the work.
    #[serde(default)]
    pub tools: Option<crate::tools::ToolsConfig>,
    /// VISION BY DELEGATION: absent (the default) means an attached image is
    /// read by the SERVING model itself, which needs a model whose volume
    /// carries a projector (`vision` below). Present points at a sibling
    /// vision deployment - normally the `image-reader` app - and then an image
    /// turn is handled by asking THAT deployment a question this app's own
    /// model writes, and folding its answer into the prompt.
    ///
    /// Which is right depends on the deployment: reading the picture locally is
    /// one model and no round trip, but it requires the serving model to be a
    /// VLM. Delegating lets the chat model be the biggest thing the share can
    /// hold and keeps the eyes on their own share, their own funding rate and
    /// their own restart button. See vision.rs for what crosses.
    #[serde(default)]
    pub vision_service: Option<crate::vision::VisionConfig>,
    /// VISION (images IN, the opposite direction from `image` above): this
    /// model volume carries a vision projector (an *mmproj*.gguf beside the
    /// weights) and the model was trained to read pictures. A per-model
    /// catalog flag rather than something probed, because /models is answered
    /// without opening an inference session and the playground has to know
    /// whether to offer the attach button before any request happens. The
    /// HOST is still the authority at request time: a deployment whose node
    /// predates the vision toolchain reports no vision through `caps` and the
    /// turn fails with that reason rather than silently ignoring the picture.
    ///
    /// ggml only. The onnx path shuttles KV through guest memory and has no
    /// image verb at all.
    #[serde(default)]
    pub vision: bool,
    /// how many prompt tokens ONE image is BUDGETED at when deciding whether
    /// a conversation still fits max_prompt_tokens. Only the host knows the
    /// true cost (a dynamic-resolution model prices an image by its grid, and
    /// M-RoPE positions differ from token counts again), so this is a
    /// deliberate over-estimate used for admission control; the real figure
    /// comes back from the host per image and is reported in the stats.
    /// Keep it at or above the node's ENCLAVE_GGML_IMAGE_MAX_TOKENS.
    #[serde(default = "default_image_tokens")]
    pub image_tokens: usize,
    /// hard cap on ONE attached image, bytes (after the browser's downscale).
    /// A phone photo is 3-8 MB; the playground resizes to ~1.2 MP before
    /// upload, which lands under 1 MB, so this is the guard against a client
    /// that does not.
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: usize,
    /// how many images one REQUEST may carry, across all its turns. Each one
    /// costs image_tokens of the window and a vision-encoder pass, so a long
    /// chat that replays ten pictures is a very different request from the one
    /// the user thinks they are sending.
    #[serde(default = "default_max_images")]
    pub max_images: usize,
}

impl AppConfig {
    /// The search leg, wherever it was configured: `tools.search` (its home
    /// since 0.39) wins over the legacy top-level block. Everything that
    /// reaches the web - the router pre-pass, the web builtins, the /search
    /// probe - resolves it through here, so the two spellings behave
    /// identically.
    pub fn search_cfg(&self) -> Option<&crate::search::SearchConfig> {
        self.tools
            .as_ref()
            .and_then(|t| t.search.as_ref())
            .or(self.search.as_ref())
    }
}

/// Reasoning budgets per rated effort, in tokens spent inside the <think>
/// block. Each is a CEILING for turns of that class, never a target, and none
/// of them may exceed the model's own think_budget - effort scaling exists to
/// spend LESS on easy turns, not to hand a model more rope than the deployment
/// signed up for.
#[derive(Deserialize, Clone)]
pub struct EffortConfig {
    /// greetings, thanks, rewrites, one-line lookups
    #[serde(default = "default_effort_low")]
    pub low: usize,
    /// ordinary explanation, short code, familiar multi-step work
    #[serde(default = "default_effort_medium")]
    pub medium: usize,
    /// proofs, tricky debugging, long derivations, careful comparison.
    /// 0 (the default) = the model's own think_budget, i.e. no reduction.
    #[serde(default)]
    pub high: usize,
    /// never cut a block shorter than this, whatever the rating says. A
    /// misjudged "low" on a question that genuinely needs a few steps should
    /// cost quality nothing: at 256 tokens a model still gets to think, it just
    /// does not get to think forever.
    #[serde(default = "default_effort_floor")]
    pub floor: usize,
}

fn default_effort_low() -> usize {
    512
}

fn default_effort_medium() -> usize {
    4096
}

fn default_effort_floor() -> usize {
    256
}

fn default_temperature() -> f32 {
    0.7
}

fn default_top_p() -> f32 {
    0.9
}

fn default_repeat_guard() -> usize {
    4
}

fn default_tokenizer() -> String {
    "local".into()
}
fn default_draft_tokens() -> usize {
    8
}

fn default_draft_p_min() -> f32 {
    0.4
}

fn default_backend() -> String {
    "onnx".into()
}

fn default_image_tokens() -> usize {
    1024
}

fn default_max_image_bytes() -> usize {
    6 * 1024 * 1024
}

fn default_max_images() -> usize {
    4
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

/// Where an image sits inside a message's text. The template renders the
/// conversation as ONE string, so an attachment leaves this mark behind and
/// the prompt is split on it afterwards into (text, image, text) runs; the
/// text runs tokenize normally and the image goes to the host as file bytes.
/// A private-use codepoint pair: no tokenizer vocabulary contains it, no
/// keyboard produces it, and build_prompt strips it out of incoming content
/// so a crafted message cannot forge an image slot it did not attach.
pub const MEDIA_MARK: &str = "\u{E000}\u{E001}";

/// A rendered prompt plus the strings that should terminate generation for
/// this template (in addition to the tokenizer-level EOS ids).
pub struct Rendered {
    pub prompt: String,
    pub stop_strings: Vec<String>,
    /// the assistant turn was force-opened with `<think>\n` - the model will
    /// not re-emit the tag, so callers prepend it to the visible output
    pub think_open: bool,
}

/// How the assistant turn opens for a <think>-trained (qwen3.x) model.
/// Mirrors the models' own jinja templates: the TEMPLATE, not the model,
/// writes the think tokens that lead every reply (chatml only).
#[derive(Clone, Copy, PartialEq)]
pub enum ThinkTurn {
    /// not a thinking model: bare assistant turn
    Plain,
    /// thinking on: force-open with `<think>\n`; the model generates the
    /// reasoning and the closing tag, never the opening one
    Open,
    /// thinking off: the trained no-think form, an empty pre-closed block
    Closed,
}

pub fn render_template(
    template: &str,
    system: &str,
    msgs: &[(String, String)], // (role, content), roles pre-filtered to user/assistant
    think: ThinkTurn,
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
            match think {
                ThinkTurn::Open => p.push_str("<think>\n"),
                ThinkTurn::Closed => p.push_str("<think>\n\n</think>\n\n"),
                ThinkTurn::Plain => {}
            }
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
    // only chatml renders think turns; elsewhere the flag stays inert
    let think_open = template == "chatml" && think == ThinkTurn::Open;
    Ok(Rendered { prompt: p, stop_strings: stops, think_open })
}
