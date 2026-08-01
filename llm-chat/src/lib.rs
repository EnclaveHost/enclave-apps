//! llm-chat: a general-purpose LLM service compiled into a wasm component,
//! running on Enclave's wasi-nn GPU interface. Ships NO weights - models
//! arrive as attached Modelwrap volumes, and every attached volume the
//! config's `models` catalog describes is servable, largest one the default
//! (see config.rs). Geometry, chat template, sampling defaults and the API
//! key are configuration, not code - a deployment can override any of it via
//! ENCLAVE_CONFIG (the platform passes the deployment's CID-verified
//! configCid JSON through the tenant environment).
//!
//! Routes:
//!   GET  /                    - chat playground (self-contained HTML).
//!   GET  /c/<chat>            - the SAME page: the open conversation is
//!                               addressed by path so a refresh or a bookmark
//!                               returns to it. The id is an IndexedDB key in
//!                               one browser profile, so the server neither
//!                               knows nor needs to know what it names.
//!   GET  /favicon.svg         - the brand mark, for whoever asks the SERVER for
//!   GET  /favicon.ico           an icon instead of reading the page's <link>
//!   GET  /apple-touch-icon.png  (crawlers, unfurlers, iOS home screens, a tab
//!                               opened straight onto a JSON route). The page
//!                               carries the mark inline, so these are never on
//!                               its own critical path.
//!   GET  /emoji.woff2         - color-emoji fallback font (Noto COLRv1): the
//!                               playground declares it with local() sources
//!                               first + unicode-range, so a browser only
//!                               downloads it when the system has no emoji
//!                               font AND a reply actually contains emoji.
//!   GET  /ping                - liveness, touches no wasi-nn.
//!   GET  /models              - the servable models (attached volumes the
//!                               config describes), largest first; the
//!                               largest is the default. Open, unlike
//!                               /v1/models - the playground dropdown reads it.
//!                               Also carries `gpu`: whether the platform gave
//!                               this deployment a GPU share (null = not
//!                               knowable here). False means the fleet had no
//!                               GPU enclave free and the app is serving in
//!                               CPU mode - the playground says so on load.
//!   GET  /warmup              - warm models before the first prompt. With
//!                               ?model=<name|volume>: that one (load + one
//!                               forward pass). BARE - the manager's boot
//!                               warmup and the playground's page load - it
//!                               is a LADDER: every servable model tried
//!                               SMALLEST-FIRST, one at a time; a model that
//!                               does not fit the share is reported unfit
//!                               and skipped, not fatal, so one published
//!                               app serves whatever the deployment can hold
//!                               (the playground disables the rest in its
//!                               menu). GPU-only unless ?target= says
//!                               otherwise.
//!   GET  /attestation         - THIS deployment's hardware attestation: the
//!                               SEV-SNP quote, its measurement, the GPU's CC
//!                               mode and nonce, relayed from the platform API
//!                               over the egress leg (attest.rs). Open, like
//!                               /models: what the hardware is has to be
//!                               readable before you decide to type into the
//!                               box. The playground's shield dialog renders it,
//!                               and re-fetches the signed document from the
//!                               enclave's own endpoint so the copy it parses
//!                               came over the BROWSER's connection.
//!   POST /title               - name a chat from its opening exchange, for a
//!                               history list. One short greedy generation,
//!                               deliberately AFTER the answer has streamed so
//!                               it is never in front of anything the user is
//!                               waiting for; a failure answers title: null and
//!                               the caller keeps its own fallback.
//!   GET  /search              - WEB SEARCH probe (503 unless the config has a
//!                               `search` block). ?q=<query> runs the provider
//!                               leg; ?url=<page> runs only the fetch+extract
//!                               leg. Two separate probes because "no results"
//!                               and "no egress" look identical from outside.
//!   GET  /v1/models           - OpenAI-compatible model list.
//!   POST /v1/chat/completions - OpenAI-compatible completions, stream and
//!                               non-stream. Point any OpenAI SDK at the
//!                               deployment URL. If the config sets api_key,
//!                               requires `Authorization: Bearer <key>`.
//!                               `enable_thinking: false` (top-level or in
//!                               chat_template_kwargs, the vLLM spelling)
//!                               turns off <think> reasoning on models whose
//!                               config marks them `thinking`. Thinking on
//!                               follows the qwen3.x templates: the prompt
//!                               force-opens the block and the server
//!                               re-emits `<think>\n` at the head of the
//!                               reply; history replays drop prior think
//!                               blocks. A config `think_budget` caps the
//!                               tokens a reply may spend in the block and
//!                               closes it if the model will not (see
//!                               ThinkGuard).
//!                               `web_search`: `"auto"` lets the MODEL decide
//!                               per turn what the question needs - a web
//!                               search, an IMAGE, or neither (one short
//!                               router generation, see route_web_search).
//!                               `true` searches every turn, absent never
//!                               does. `/search ` forces a search and
//!                               `/image ` forces a picture, regardless.
//!                               Sources come back on `enclave.search`, or as
//!                               an SSE comment `: enclave-search {...}`
//!                               streaming. Image generation needs the
//!                               config's `image` block (see image.rs) and is
//!                               delivered to /chat as an `{"image":{...}}`
//!                               event carrying a data: URI.
//!                               IMAGES IN: a message's `content` may be
//!                               OpenAI's array of parts, with attachments as
//!                               base64 data: URIs. They are read by the
//!                               MODEL, on models whose volume carries a
//!                               vision projector and whose config says
//!                               `vision` (see the vision section below).
//!                               TOOLS, two modes told apart by the `tools`
//!                               field's shape. `tools: true` lets the model
//!                               call the endpoints and MCP servers THIS
//!                               DEPLOYMENT configured (see tools.rs and the
//!                               config's `tools` block); what ran comes back
//!                               on `enclave.tools`, or as SSE comments
//!                               `: enclave-tool {...}` while streaming. An
//!                               OpenAI `tools: [...]` ARRAY is the
//!                               PASSTHROUGH: the client's own functions are
//!                               offered to the model INSTEAD of the
//!                               deployment's registry, its call comes back as
//!                               `tool_calls` with finish_reason
//!                               "tool_calls", and the CLIENT executes it -
//!                               nothing here runs a caller's tool. Send the
//!                               result as a `role: "tool"` message (agent
//!                               frameworks do this on their own). In both
//!                               modes `tool_choice: "none"` withholds the
//!                               lot; `"required"` and the named-function form
//!                               are honoured as an instruction in the prompt
//!                               (there is no grammar constraint here).
//!   POST /chat                - legacy SSE endpoint used by the playground.
//!                               Same `web_search` switch; sources arrive as a
//!                               `{"search":{...}}` event before the first
//!                               token. Tool calls arrive as `{"tool":{...}}`
//!                               (before the round trip) and
//!                               `{"tool_result":{...}}` (after it); the reply
//!                               regenerates from the result, so a `tool` event
//!                               resets the client's buffer the way a `notice`
//!                               does.
//!   GET  /tools               - operator probe: resolve the registry (which
//!                               DIALS any MCP server) and show what a turn
//!                               would be offered, with `?call=<name>&args=<json>`
//!                               to run one. The counterpart of /search?q=.
//!
//! Generation: autoregressive decode with the model's KV cache. The trick
//! that makes this cheap through wasi-nn: `compute()` returns OWNED tensor
//! resources for the `present.*` KV tensors, and we hand those handles
//! straight back as the next step's `past_key_values.*` inputs - the cache
//! bytes never cross into guest memory. Only the logits are read out
//! (one vocab row per decode step).
//!
//! VISION (ggml, models whose volume pairs the weights with an *mmproj*.gguf
//! projector and whose catalog entry sets `vision`): a turn's attachments are
//! carried to the model as PICTURES, not as a description of one. The prompt
//! stops being a flat token list and becomes runs of text with images spliced
//! between them (PromptPart): the chat template renders as usual with a
//! private mark where each image goes, the rendered string is cut at those
//! marks, the text runs tokenize normally, and each image crosses to the host
//! as its raw FILE BYTES through the wasi-nn "image" verb. The host encodes it
//! and answers with the POSITIONS it consumed - which the guest cannot derive,
//! since a dynamic-resolution model prices an image by its own grid and M-RoPE
//! numbers image positions differently again. WebP is the one exception to
//! "raw file bytes": the host's encoder has no VP8, so a webp is decoded and
//! re-encoded as JPEG on the way in (see webp.rs).
//!
//! Nothing about any particular VLM lives here as a result: the marker tokens
//! that wrap an image, the non-causal mask some models want around it, the
//! 2-D position arithmetic - all of that is llama.cpp's, behind the host. What
//! this app owns is the parts that are its own: which requests are allowed to
//! carry pictures (check_images), what one costs against the context window,
//! and the fact that speculative decoding sits out any turn with an image in
//! it, because its bookkeeping counts one position per token.
//!
//! VISION BY DELEGATION (the config's `vision_service` block, see vision.rs):
//! the other way to answer a picture, for a deployment whose chat model is
//! bigger than any VLM it could afford to attach beside it. The image goes to a
//! sibling `image-reader` deployment and comes back as prose, which is folded into
//! the turn. What crosses is NOT the conversation: one image, and a QUESTION
//! that THIS deployment's model writes after reading the conversation - so the
//! detail the answer depends on ("the spec said two buttons") travels as part of
//! the question rather than as a transcript. Costs one extra short generation
//! and one round trip, and the look is single-shot: there is no tool-call loop
//! in the render path, so the question asks for the specific answer AND enough
//! surrounding description to survive the obvious follow-up.
//!
//! The two paths are exclusive per turn and vision_plan() decides: a request
//! that NAMES a vision model reads locally, everything else prefers the service
//! when one is configured. With a service configured, EVERY model in the
//! catalog can be sent a picture, which is what /models reports as
//! `vision.service` so the playground stops gating the attach button on the
//! selected model.
#[allow(warnings)]
mod bindings;

mod attest;
mod config;
mod http;
mod image;
mod sampling;
mod search;
mod tools;
mod vision;
mod webp;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokenizers::Tokenizer;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::nn::graph::{load, load_by_name, ExecutionTarget, GraphEncoding};
use bindings::wasi::nn::inference::GraphExecutionContext;
use bindings::wasi::nn::tensor::{Tensor, TensorType};

use config::AppConfig;
use sampling::{pick_row, Rng, Row, SampleParams};

static CHAT_HTML: &str = include_str!("chat.html");
static EMOJI_WOFF2: &[u8] = include_bytes!("../assets/emoji.woff2");
/// The brand mark, for the consumers that ask the SERVER for an icon rather
/// than reading the page's <link>: browsers hitting a non-HTML route, crawlers,
/// link unfurlers, bookmark managers, and iOS adding a home-screen tile. The
/// page itself carries the mark inline, so none of these are on its critical
/// path. SVG for anything that will take it, a 32px PNG inside an ICO container
/// for /favicon.ico, and a 180px full-bleed PNG for iOS - which masks the icon
/// with its own squircle, so a transparent rounded corner would come out black.
static FAVICON_SVG: &str = include_str!("../assets/eyesoff.svg");
static FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");
static TOUCH_ICON_PNG: &[u8] = include_bytes!("../assets/apple-touch-icon.png");
/// The installable-app shell: a manifest so browsers offer install, PNG icons
/// at the launcher sizes (plus a full-bleed maskable), and a service worker
/// that caches the whole static shell for offline. The worker's cache is keyed
/// on ASSET_REV (build.rs hashes every shell byte into it), which is what lets
/// a stable custom domain swap versions without serving a stale shell.
static MANIFEST_JSON: &[u8] = include_bytes!("../assets/manifest.webmanifest");
static SW_JS: &str = include_str!("sw.js");
static ICON_192_PNG: &[u8] = include_bytes!("../assets/icon-192.png");
static ICON_512_PNG: &[u8] = include_bytes!("../assets/icon-512.png");
static ICON_MASKABLE_PNG: &[u8] = include_bytes!("../assets/icon-maskable-512.png");
static ASSET_REV: &str = env!("ASSET_REV");

// ------------------------------------------------------------ model volumes --
// Weights + tokenizer arrive as ATTACHED MODEL VOLUMES (Tinfoil Modelwrap):
// the platform preopens each attached volume read-only at /models/<name> and
// lists the names in ENCLAVE_MODELS. The app embeds NO weights - the config's
// `models` catalog describes the volumes it can serve (see config.rs and
// available_models below), so ONE published wasm serves whatever the
// deployment mounts. The host caches the ORT session by graph bytes and
// preloads GGUF graphs at boot, so re-reading per request only pays real
// cost on the first load after a node boot.
const MODELS_ROOT: &str = "/models";

fn attached_volumes() -> Vec<String> {
    std::env::var("ENCLAVE_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The volume names whose graphs the manager put on the wasmtime cmdline at
/// tenant boot (ENCLAVE_NN_PRELOADS). The graph registry is sealed at process
/// start, so this is the complete list of names load_by_name() can ever find:
/// a NotFound on a listed name means the boot preload FAILED (loudly, in the
/// tenant log); on an unlisted name the host never tried (volume mounted
/// late, over the VRAM budget, or no unambiguous file). None = the manager
/// predates the env - no signal, keep the generic diagnosis.
fn preloaded_graphs() -> Option<Vec<String>> {
    std::env::var("ENCLAVE_NN_PRELOADS").ok().map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// Resolve a config path against the model volume: absolute paths are used
/// verbatim (cross-VOLUME reads - e.g. a tokenizer living in a sibling
/// volume when the weights repo carries none), everything else is
/// volume-relative.
fn volume_path(root: &PathBuf, rel: &str) -> PathBuf {
    if rel.starts_with('/') { PathBuf::from(rel) } else { root.join(rel) }
}

/// Read one file out of the configured model volume. The `explicit` config
/// path is tried first, then the conventional candidate names - a fallback,
/// not a replacement, so one catalog entry can serve both a volume that
/// carries the file and one that needs the explicit (possibly cross-volume)
/// location.
fn volume_file(
    cfg: &AppConfig,
    explicit: &Option<String>,
    candidates: &[&str],
    what: &str,
) -> Result<Vec<u8>, String> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    if !root.is_dir() {
        let have = attached_volumes();
        return Err(format!(
            "model volume '{}' is not attached at {MODELS_ROOT}/{} (attached: {}) - \
             deploy with {{\"volumes\":[\"{}\"]}} in the config, or tick it in the console's volume picker",
            cfg.model_volume,
            cfg.model_volume,
            if have.is_empty() { "none".to_string() } else { have.join(", ") },
            cfg.model_volume,
        ));
    }
    let mut rels: Vec<String> = Vec::new();
    rels.extend(explicit.iter().cloned());
    rels.extend(candidates.iter().map(|s| s.to_string()));
    for rel in &rels {
        let p = volume_path(&root, rel);
        if p.is_file() {
            return std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display()));
        }
    }
    Err(format!(
        "no {what} in volume '{}' (tried: {})",
        cfg.model_volume,
        rels.join(", ")
    ))
}

const ONNX_CANDIDATES: &[&str] =
    &["onnx/model_q4.onnx", "model_q4.onnx", "onnx/model.onnx", "model.onnx"];

fn read_model(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    volume_file(cfg, &cfg.model_file, ONNX_CANDIDATES, "ONNX model")
}

fn read_tokenizer(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    volume_file(cfg, &cfg.tokenizer_file, &["tokenizer.json"], "tokenizer.json")
}

/// The split-GGUF family covering `path` (llama.cpp's
/// "<prefix>-NNNNN-of-MMMMM.gguf" convention, forced on >50GB models by HF's
/// per-file cap), every part present - or None if `path` isn't a split part
/// or a sibling is missing. The host loads part 00001 and derives the sibling
/// paths from its name; the model's true size is the sum of its parts.
fn split_family(path: &Path) -> Option<Vec<PathBuf>> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".gguf")?;
    let (rest, count) = stem.rsplit_once("-of-")?;
    let (prefix, no) = rest.rsplit_once('-')?;
    if no.len() != 5 || count.len() != 5 || no.parse::<u32>().is_err() {
        return None;
    }
    let n = count.parse::<u32>().ok().filter(|n| *n >= 1)?;
    let dir = path.parent()?;
    let parts: Vec<PathBuf> = (1..=n)
        .map(|i| dir.join(format!("{prefix}-{i:05}-of-{count}.gguf")))
        .collect();
    parts.iter().all(|p| p.is_file()).then_some(parts)
}

/// Locate `cfg`'s weights file WITHOUT reading it: powers the availability
/// listing and the size ranking that picks the default model. Mirrors the
/// lookup the backends do for real. For ggml, `model_file` names the gguf in
/// a multi-quant volume (keep it in step with the host's MODEL_VOLUMES pick -
/// the host decides what actually preloads); otherwise the host's
/// model.gguf / single-*.gguf / one-split-family contract applies. A split
/// model reports the SUM of its parts - part 00001 alone is a header-sized
/// sliver, and this ranking picks the default model.
fn weights_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    if cfg.backend == "ggml" {
        let path = if let Some(f) = &cfg.model_file {
            volume_path(&root, f)
        } else {
            let preferred = root.join("model.gguf");
            if preferred.is_file() {
                preferred
            } else {
                let mut ggufs: Vec<PathBuf> = std::fs::read_dir(&root)
                    .ok()?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false) && p.is_file())
                    // a vision volume's SECOND gguf is the projector, not a
                    // second model - the host tells them apart by this same
                    // name convention, and counting it here would make every
                    // vision volume look ambiguous and drop out of the listing
                    .filter(|p| !is_mmproj(p))
                    .collect();
                match ggufs.len() {
                    0 => return None,
                    1 => ggufs.pop()?,
                    n => {
                        // one complete split family covering every gguf, else ambiguous
                        let first = ggufs
                            .iter()
                            .find(|p| {
                                split_family(p).is_some_and(|fam| fam.len() == n)
                            })?
                            .clone();
                        first
                    }
                }
            }
        };
        let parts = split_family(&path);
        let files = parts.as_deref().unwrap_or(std::slice::from_ref(&path));
        let mut total = 0u64;
        for f in files {
            total += std::fs::metadata(f).ok()?.len();
        }
        return Some(total);
    }
    let mut rels: Vec<String> = Vec::new();
    rels.extend(cfg.model_file.iter().cloned());
    rels.extend(ONNX_CANDIDATES.iter().map(|s| s.to_string()));
    rels.iter()
        .map(|rel| volume_path(&root, rel))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
}

// ------------------------------------------------------------ model choice --

/// One servable model: an attached volume the config knows how to describe.
struct ModelEntry {
    volume: String,
    bytes: u64,
    cfg: AppConfig,
}

/// The servable models: every attached volume with a `models` catalog entry
/// (or equal to the top-level model_volume, which the top-level config
/// describes by itself). Sorted by weights size, LARGEST FIRST - index 0 is
/// the default, so a deployment that attaches several models serves the
/// biggest unless a request names another. Volumes that are attached but
/// undescribed (unknown geometry/template) or missing their weights file are
/// skipped - they cannot be served, only misserved.
fn available_models(raw: &serde_json::Value) -> Vec<ModelEntry> {
    let top_volume = raw.get("model_volume").and_then(|v| v.as_str()).unwrap_or("");
    let catalog = raw.get("models").and_then(|m| m.as_object());
    let mut out = Vec::new();
    for vol in attached_volumes() {
        let entry = match catalog.and_then(|m| m.get(&vol)) {
            Some(e) => e.clone(),
            None if vol == top_volume => serde_json::json!({}),
            None => continue,
        };
        let Ok(cfg) = config::resolve_entry(raw, &vol, entry) else { continue };
        let Some(bytes) = weights_size(&cfg) else { continue };
        out.push(ModelEntry { volume: vol, bytes, cfg });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.volume.cmp(&b.volume)));
    out
}

/// The deployment's VRAM budget in bytes: ENCLAVE_VRAM_BYTES, set by the
/// platform from gpuShare x card VRAM - the same number the MPS cap
/// enforces on this process. None on CPU deployments and older managers.
fn vram_budget() -> Option<u64> {
    std::env::var("ENCLAVE_VRAM_BYTES")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|b| *b > 0)
}

/// Whether this deployment holds GPU resources at all - the fact the
/// playground's CPU-mode notice is built on.
///
/// ENCLAVE_VRAM_BYTES is set from gpuShare x card VRAM on every GPU
/// deployment and left unset on a CPU one, so a value is PROOF of a share -
/// and its ABSENCE, inside a tenant environment, is the CPU signal. The one
/// guard is that a tenant environment exists at all: some ENCLAVE_*
/// variable must be present, or this is a dev box and nothing is known.
///
/// Deliberately does NOT consult ENCLAVE_NN_PRELOADS. It used to, reading a
/// preload as proof of a GPU node on the theory that only a GPU share can
/// put a GGUF in VRAM. The fleet's CPU-serving path preloads ggml graphs
/// too (into host RAM) - that is what makes CPU serving work - so the test
/// swallowed the exact case this exists for: metal0 served on CPU and
/// reported "unknown", and the playground said nothing. The residual risk
/// is the mirror image and much cheaper: a manager that sets no VRAM budget
/// on a GPU node shows an informational notice it did not need.
///
/// Some(true) = GPU share; Some(false) = tenant with no GPU, i.e. CPU mode;
/// None = not a platform tenant, and the playground stays quiet.
fn gpu_present() -> Option<bool> {
    if vram_budget().is_some() {
        return Some(true);
    }
    std::env::vars()
        .any(|(k, _)| k.starts_with("ENCLAVE_"))
        .then_some(false)
}

/// Bytes per KV-cache element, in SIXTEENTHS (q8_0 stores 34 bytes per
/// 32-element block = 17/16 bytes each; f16 = 32/16; and so on).
fn kv_elem_sixteenths(t: &str) -> u64 {
    match t {
        "f32" => 64,
        "q8_0" => 17,
        "q4_0" => 9,
        "q4_1" => 10,
        _ => 32, // f16 / bf16 / unknown
    }
}

/// Estimated VRAM to SERVE one ggml model beyond its resident weights: the
/// KV cache at the node's context window plus a flat working-set allowance
/// (compute buffers, FA workspace, CUDA context). llama.cpp allocates the
/// FULL window up front regardless of prompt length and does NOT clamp to
/// the model's training window, so the window term dominates. The node
/// tuning (ENCLAVE_GGML_N_CTX + KV cache types) is forwarded by the
/// manager; when absent (older managers, dev boxes) this returns 0 and the
/// budget gate degrades to weights-only, as before. Returns
/// (kv_bytes, working_set) - callers sum them.
const WORKING_SET: u64 = 3 << 29; // 1.5 GiB, deliberately round
fn serve_cost(cfg: &AppConfig) -> (u64, u64) {
    if cfg.backend != "ggml" {
        return (0, 0);
    }
    let Some(n_ctx) = std::env::var("ENCLAVE_GGML_N_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    else {
        return (0, 0);
    };
    let tk = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").unwrap_or_default();
    let tv = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").unwrap_or_else(|_| tk.clone());
    let elems = cfg.kv_layers.unwrap_or(cfg.n_layers) as u64
        * cfg.n_kv_heads as u64
        * cfg.head_dim as u64;
    let kv = elems * n_ctx * (kv_elem_sixteenths(tk.trim()) + kv_elem_sixteenths(tv.trim())) / 16;
    // A VISION model's projector is resident VRAM too, once an image has been
    // sent: the encoder weights plus its own compute buffers. It is loaded
    // lazily, which is exactly why it has to be priced HERE - a deployment
    // that fits the language model and nothing else would load fine, serve
    // text fine, and then die on the first picture (a CUDA OOM inside compute
    // aborts the whole tenant, taking every model with it). The projector
    // file sits in the volume, so its real size is knowable rather than
    // guessed; the allowance on top covers the encode workspace.
    let vision = if cfg.vision { mmproj_size(cfg).unwrap_or(1 << 30) + (1 << 29) } else { 0 };
    if pooled_backend() {
        // Continuous-batching host: ONE shared KV pool of n_ctx tokens per
        // model serves every concurrent session - the pool prices once,
        // regardless of ENCLAVE_GGML_MAX_SESSIONS (that only caps sequences
        // sharing it).
        return (kv + vision, WORKING_SET);
    }
    // Pre-batching host: up to ENCLAVE_GGML_MAX_SESSIONS concurrent contexts,
    // EACH with its own full window + working set - price the worst case.
    // Absent env = 1, matching that host's gate default (and the pre-gate
    // reality only by assumption, which is what broke 2026-07-24).
    let sessions = std::env::var("ENCLAVE_GGML_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    (kv * sessions + vision, WORKING_SET * sessions)
}

/// Does this file name mark a vision projector rather than a model? The HOST
/// picks the projector out of a volume by exactly this convention, so the two
/// sides must agree or they will disagree about which gguf is the model.
fn is_mmproj(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("mmproj"))
        .unwrap_or(false)
}

/// The vision projector's size in the model volume, if it carries one. Same
/// name convention the host matches on (*mmproj*.gguf), so the two agree
/// about which file this is.
fn mmproj_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false) && is_mmproj(p))
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
        .max()
}

/// The manager sets ENCLAVE_GGML_POOLED on hosts whose ggml backend serves
/// all concurrent sessions from one shared KV pool per model (continuous
/// batching). It changes the serve-cost SHAPE: pools price once per model
/// and PERSIST once a model has served (the host keeps the server for the
/// graph's lifetime), instead of transient per-request windows.
fn pooled_backend() -> bool {
    std::env::var("ENCLAVE_GGML_POOLED")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

/// Which servable models CANNOT serve within the VRAM budget. Models claim
/// the budget smallest-first (the preload / warmup order, same tie-break as
/// the manager's emission), each needing its weights RESIDENT plus - while
/// a session is open - its KV cache at the node's window and a working-set
/// allowance (serve_cost). Refusing here is load-bearing, not cosmetic: a
/// CUDA OOM inside compute ABORTS the whole wasmtime process (ggml_abort -
/// no error reaches the guest, every model goes down with it), so a
/// too-big model must never be probed, let alone served. Serve cost covers
/// the host's whole concurrency model: pooled hosts (ENCLAVE_GGML_POOLED)
/// price one persistent KV pool per model and ACCUMULATE it across models,
/// pre-pooling hosts price ENCLAVE_GGML_MAX_SESSIONS transient windows for
/// the served model only; when the node env is unknown the estimate
/// degrades to weights-only. Returns volume -> reason, unfit models only.
fn over_budget(entries: &[ModelEntry]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(budget) = vram_budget() else { return out };
    let mut asc: Vec<&ModelEntry> = entries.iter().collect();
    asc.sort_by(|a, b| a.bytes.cmp(&b.bytes).then(a.volume.cmp(&b.volume)));
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut claimed = 0u64;
    for e in asc {
        let (kv, ws) = serve_cost(&e.cfg);
        let fits = claimed + e.bytes + kv + ws <= budget;
        if fits {
            // weights stay resident once preloaded; on a pooled host the KV
            // pool and working set ALSO persist from the moment a model first
            // serves (the boot warmup ladder touches every fitting model), so
            // they claim budget from the next model too
            claimed += e.bytes + if pooled_backend() { kv + ws } else { 0 };
            continue;
        }
        {
            let need = e.bytes + kv + ws;
            out.insert(
                e.volume.clone(),
                if kv > 0 {
                    format!(
                        "needs ~{:.1} GB to serve ({:.1} GB weights + {:.1} GB KV cache at the \
                         node's context window + working set) but {:.1} GB of the {:.1} GB VRAM \
                         budget remains - redeploy with a larger GPU share to unlock this model",
                        gb(need),
                        gb(e.bytes),
                        gb(kv),
                        gb(budget.saturating_sub(claimed)),
                        gb(budget)
                    )
                } else {
                    format!(
                        "{:.1} GB of weights cannot fit the deployment's {:.1} GB VRAM budget\
                         {} - redeploy with a larger GPU share to unlock this model",
                        gb(e.bytes),
                        gb(budget),
                        if claimed > 0 {
                            format!(" ({:.1} GB already claimed by smaller models)", gb(claimed))
                        } else {
                            String::new()
                        }
                    )
                },
            );
        }
    }
    out
}

/// The AppConfig serving one request. `requested` (the OpenAI `model` field,
/// or ?model= on /warmup) matches a model name or volume name; an UNKNOWN
/// name falls back to the default model instead of erroring - OpenAI SDKs
/// require a model string and clients routinely send one this deployment
/// never heard of. A KNOWN name the VRAM budget cannot serve is REFUSED
/// with the reason (attempting it can abort the whole tenant - see
/// over_budget); the default pick is the largest attached model that FITS,
/// falling back to the plain largest when nothing fits or the budget is
/// unknown. No servable models at all falls back to the top-level config,
/// whose volume-not-attached error path tells the operator what to attach.
fn resolve_model(raw: &serde_json::Value, requested: Option<&str>) -> Result<AppConfig, String> {
    let entries = available_models(raw);
    let unfit = over_budget(&entries);
    if let Some(want) = requested {
        if let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) {
            if let Some(why) = unfit.get(&e.volume) {
                return Err(format!("model '{want}' cannot serve on this deployment: {why}"));
            }
            return Ok(e.cfg.clone());
        }
    }
    if let Some(e) = entries.iter().find(|e| !unfit.contains_key(&e.volume)) {
        return Ok(e.cfg.clone());
    }
    match entries.into_iter().next() {
        Some(e) => Ok(e.cfg),
        None => config::from_value(raw.clone()),
    }
}

/// How this generation's tokens get proposed for verification.
enum DraftPlan {
    /// no drafting - plain decode
    Plain,
    /// a same-tokenizer sibling model proposes (its resolved config)
    Model(AppConfig),
    /// the model's own MTP head proposes (host-side; no second model)
    Mtp,
    /// prompt-lookup: n-gram matches against the conversation itself propose
    /// (no model at all - free drafts; rounds without a match decode plain)
    Lookup,
}

/// The draft plan for speculative decoding, when `cfg` names one and it is
/// usable - else Plain plus the reason for a status line. "mtp" = the
/// model's own head (existence verified against the host at session open).
/// For a MODEL draft the tokenizer must be IDENTICAL (byte-equal
/// tokenizer.json): draft token ids are meaningless in a foreign
/// vocabulary, and differing merge tables corrupt silently. Two volumes
/// shipping cosmetically different copies of the same tokenizer can be
/// forced compatible by pointing the draft entry's tokenizer_file at the
/// target's copy (absolute /models/... paths work).
fn resolve_draft(raw: &serde_json::Value, cfg: &AppConfig) -> (DraftPlan, Option<String>) {
    let Some(want) = cfg.draft.as_deref() else { return (DraftPlan::Plain, None) };
    if cfg.backend != "ggml" {
        return (DraftPlan::Plain, Some("speculative decoding needs the ggml backend".into()));
    }
    if want == "mtp" {
        // whether the loaded GGUF actually carries a head is the host's
        // knowledge - probed via caps at session open, which falls back
        // with its own note if not
        return (DraftPlan::Mtp, None);
    }
    if want == "lookup" {
        // needs nothing beyond the ggml branch-verify verbs (any model,
        // no head, no second volume)
        return (DraftPlan::Lookup, None);
    }
    let entries = available_models(raw);
    let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) else {
        return (DraftPlan::Plain, Some(format!("draft model '{want}' is not attached (or not in the models catalog)")));
    };
    if e.cfg.backend != "ggml" {
        return (DraftPlan::Plain, Some(format!("draft '{want}' is not a ggml model")));
    }
    if e.cfg.vocab != cfg.vocab {
        return (DraftPlan::Plain, Some(format!(
            "draft '{want}' speaks a different vocabulary ({} vs {}) - a draft must share the              target's tokenizer exactly (qwen3.x models share one; qwen2.5-0.5b does not)",
            e.cfg.vocab, cfg.vocab
        )));
    }
    match (read_tokenizer(cfg), read_tokenizer(&e.cfg)) {
        (Ok(a), Ok(b)) if a == b => {}
        (Ok(_), Ok(_)) => {
            return (DraftPlan::Plain, Some(format!(
                "draft '{want}' ships a different tokenizer.json than the target - set the draft                  entry's tokenizer_file to the target's copy (an absolute /models/... path) if                  they are really the same tokenizer"
            )))
        }
        _ => return (DraftPlan::Plain, Some("couldn't read both tokenizers to verify draft compatibility".into())),
    }
    if over_budget(&entries).contains_key(&e.volume) {
        return (DraftPlan::Plain, Some(format!("draft '{want}' does not fit this deployment's share")));
    }
    (DraftPlan::Model(e.cfg.clone()), None)
}

const PREFILL_CHUNK: usize = 128;
/// Request-body ceiling. Generous because attachments arrive base64'd INSIDE
/// the JSON (~1.35x the file), and a vision turn can legitimately carry
/// several: max_images * max_image_bytes * 1.35 has to fit, plus the
/// conversation around it. The per-image and per-request limits in
/// check_images are the real policy; this is only the wall that stops a body
/// from being read into guest memory at all.
const MAX_BODY_BYTES: usize = 40 * 1024 * 1024;

/// The host's ggml session gate (ENCLAVE_GGML_MAX_SESSIONS) tags its
/// fail-fast error with this marker. Every llama context pre-allocates the
/// FULL n_ctx KV window, so the host caps live contexts and this app QUEUES
/// (wait-and-retry below) instead of stacking window allocations until the
/// share's VRAM dies — the 2026-07-24 many-users outage, which surfaced as
/// "cpu: llama.cpp context allocation failed (n_ctx too large for the
/// share?)" on every request.
const BUSY_MARKER: &str = "[sessions_busy]";
const BUSY_POLL_MS: u64 = 2000;
const BUSY_WAIT_BUDGET_MS: u128 = 300_000; // stop queueing after 5 minutes
/// generate()'s queue keepalive opens with this, and internal_status
/// prefix-matches it to tell the wait ticks from the load/ready lines
/// around them.
const BUSY_STATUS: &str = "all inference sessions are busy";
/// Busy-queue allowance for the INTERNAL generations (the router verdict, the
/// vision question, the chat title): long enough to ride out a normal turn
/// finishing ahead, far short of the main leg's five minutes. An optional
/// pass that cannot start is dropped, so the queue allowance stays with the
/// answer the user is actually waiting for.
const INTERNAL_BUSY_BUDGET_MS: u128 = 30_000;

/// Status relay for an internal generation (router verdict, vision question,
/// chat title). generate() narrates its busy queue through the status
/// callback, and these passes used to hand it a no-op - which held the turn
/// SILENT while they queued on a saturated node. Bytes are what keep a stream
/// alive: measured on this fleet 2026-07-31, ~180s without one and the
/// gateway cuts the response mid-turn, which the playground reports as "the
/// instance likely restarted" - and a retry meets the same queue. So the wait
/// ticks are forwarded under the leg's own label, and once the queue has
/// eaten `budget_ms` the relay returns false, which generate() already treats
/// as "stop waiting"; every caller of these passes falls back the way it does
/// on any other failure. The load/ready lines are swallowed: mid-leg they
/// would only garble the narration.
fn internal_status<'a>(
    label: &'static str,
    on_status: &'a dyn Fn(&str),
    budget_ms: u128,
) -> impl Fn(&str) -> bool + 'a {
    let t0 = now_ms();
    move |s: &str| {
        if !s.starts_with(BUSY_STATUS) {
            return true;
        }
        let waited = now_ms() - t0;
        if waited >= budget_ms {
            return false;
        }
        on_status(&format!("{label} (waiting for a free inference slot, {}s)", waited / 1000));
        true
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Sleep on the monotonic clock's pollable — parks this request in the host's
/// poll loop (a spin wait would burn the share's cpu slice for nothing).
fn sleep_ms(ms: u64) {
    let p = bindings::wasi::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000);
    bindings::wasi::io::poll::poll(&[&p]);
}

// ---------------------------------------------------------------- tensors --

fn i64_tensor(dims: &[u32], vals: &[i64]) -> Tensor {
    let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
    Tensor::new(dims, TensorType::I64, &bytes)
}

fn empty_past(cfg: &AppConfig) -> Vec<(String, Tensor)> {
    let mut past = Vec::with_capacity((cfg.n_layers * 2) as usize);
    for l in 0..cfg.n_layers {
        for kind in ["key", "value"] {
            past.push((
                format!("past_key_values.{l}.{kind}"),
                Tensor::new(&[1, cfg.n_kv_heads, 0, cfg.head_dim], TensorType::Fp32, &[]),
            ));
        }
    }
    past
}

fn nn_err(stage: &str, e: bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

/// Diagnose a failed ggml load_by_name(). A NotFound is NOT one condition -
/// the graph registry seals at process start, so it splits three ways, and
/// telling users the wrong one ("volume missing" for a model the platform is
/// about to load) was exactly the confusion seen live 2026-07-18:
///   volume dir absent        -> operator error: attach the volume
///   name in ENCLAVE_NN_PRELOADS -> the boot preload FAILED (tenant log)
///   name not in it           -> the host never tried: mounted late (the
///                               platform detects and restarts) or over the
///                               VRAM budget (the ladder marks it unfit)
/// The [code] marker rides in the message; json_err lifts it into the error
/// object's machine-readable `code` and strips it from the text.
fn ggml_load_err(cfg: &AppConfig, e: bindings::wasi::nn::errors::Error) -> String {
    use bindings::wasi::nn::errors::ErrorCode;
    let not_found = matches!(e.code(), ErrorCode::NotFound);
    let base = nn_err("load_by_name", e);
    let vol = &cfg.model_volume;
    if !not_found {
        return format!("{base} (loading \"{vol}\")");
    }
    if !PathBuf::from(MODELS_ROOT).join(vol).is_dir() {
        return format!(
            "[volume_not_attached] {base} - the \"{vol}\" volume is not attached; deploy \
             with {{\"volumes\":[\"{vol}\"]}} in the config, or tick it in the console's \
             volume picker"
        );
    }
    match preloaded_graphs() {
        Some(pre) if pre.iter().any(|p| p == vol) => format!(
            "[host_load_failed] {base} - the host tried to load \"{vol}\" when this \
             deployment started and FAILED (the deployment log has the reason); if this \
             persists, the deployment's share cannot hold the model"
        ),
        Some(_) => format!(
            "[model_not_loaded] {base} - \"{vol}\" is attached but was not loaded when \
             this deployment started (the volume finished mounting later, or it exceeds \
             the share's VRAM budget); the platform restarts the deployment to load it - \
             retry shortly"
        ),
        None => format!(
            "{base} (is the \"{vol}\" volume attached, and does it carry a GGUF? \
             ggml needs a GPU-share deployment - the host preloads the model)"
        ),
    }
}

// ------------------------------------------------------------- generation --

struct StepResult {
    logits: Vec<f32>,
    past: Vec<(String, Tensor)>,
}

/// One forward pass. `past` is consumed (the host drops the old cache).
fn step(
    cfg: &AppConfig,
    ctx: &GraphExecutionContext,
    ids: &[i64],
    past: Vec<(String, Tensor)>,
    past_len: usize,
    read_logits: bool,
) -> Result<StepResult, String> {
    let new_len = ids.len();
    let total = past_len + new_len;
    let mut inputs: Vec<(String, Tensor)> = Vec::with_capacity(3 + past.len());
    inputs.push(("input_ids".into(), i64_tensor(&[1, new_len as u32], ids)));
    inputs.push((
        "attention_mask".into(),
        i64_tensor(&[1, total as u32], &vec![1i64; total]),
    ));
    let positions: Vec<i64> = (past_len as i64..total as i64).collect();
    inputs.push((
        "position_ids".into(),
        i64_tensor(&[1, new_len as u32], &positions),
    ));
    inputs.extend(past);

    let outputs = ctx.compute(inputs).map_err(|e| nn_err("compute", e))?;

    let mut logits = Vec::new();
    let mut next_past = Vec::with_capacity((cfg.n_layers * 2) as usize);
    for (name, tensor) in outputs {
        if name == "logits" {
            if read_logits {
                let data = tensor.data();
                let row = cfg.vocab * 4;
                if data.len() < row {
                    return Err(format!("logits too short: {} bytes", data.len()));
                }
                let tail = &data[data.len() - row..];
                logits = tail
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
        } else if let Some(rest) = name.strip_prefix("present.") {
            next_past.push((format!("past_key_values.{rest}"), tensor));
        }
    }
    if next_past.len() != (cfg.n_layers * 2) as usize {
        return Err(format!(
            "expected {} KV outputs, got {} - do the config's n_layers/n_kv_heads match the model?",
            cfg.n_layers * 2,
            next_past.len()
        ));
    }
    if read_logits && logits.len() != cfg.vocab {
        return Err("model returned no logits (config vocab mismatch?)".into());
    }
    Ok(StepResult { logits, past: next_past })
}

/// A live inference session. The two backends differ in WHERE the KV cache
/// lives: ONNX shuttles past_key_values tensors through every call (the cache
/// crosses the guest boundary), while ggml keeps it host-side inside the
/// execution context - the guest feeds token ids and reads one logits row.
/// ggml models are HOST-PRELOADED (-S nn-graph=ggml::<volume dir>), so
/// load_by_name(model_volume) never pulls weights into guest memory and the
/// model size is bounded by the deployment's share, not wasm32.
enum Session {
    Onnx { ctx: GraphExecutionContext, past: Vec<(String, Tensor)>, total: usize },
    Ggml { ctx: GraphExecutionContext },
}

impl Session {
    fn open(cfg: &AppConfig, target: ExecutionTarget) -> Result<Session, String> {
        match cfg.backend.as_str() {
            "ggml" => {
                let graph = load_by_name(&cfg.model_volume).map_err(|e| ggml_load_err(cfg, e))?;
                let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
                Ok(Session::Ggml { ctx })
            }
            "onnx" => {
                let model = read_model(cfg)?;
                let graph =
                    load(&[model], GraphEncoding::Onnx, target).map_err(|e| nn_err("load", e))?;
                let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
                Ok(Session::Onnx { ctx, past: empty_past(cfg), total: 0 })
            }
            other => Err(format!("unknown backend \"{other}\" (expected \"onnx\" or \"ggml\")")),
        }
    }

    /// Feed `ids`; with `want_logits`, return the LAST token's logits row.
    fn feed(&mut self, cfg: &AppConfig, ids: &[u32], want_logits: bool) -> Result<Row, String> {
        match self {
            Session::Onnx { ctx, past, total } => {
                let ids64: Vec<i64> = ids.iter().map(|&t| t as i64).collect();
                let r = step(cfg, ctx, &ids64, std::mem::take(past), *total, want_logits)?;
                *past = r.past;
                *total += ids.len();
                Ok(Row::dense(r.logits))
            }
            Session::Ggml { ctx } => {
                let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
                let mut inputs = vec![(
                    "tokens".to_string(),
                    Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes),
                )];
                if want_logits {
                    inputs.push(topk_input());
                }
                inputs.push(timing_input());
                let outs = ctx.compute(inputs).map_err(|e| nn_err("compute", e))?;
                note_timing("feed", &outs);
                if !want_logits {
                    return Ok(Row::dense(Vec::new()));
                }
                let mut rows = rows_from_outs(&outs, 1, cfg.vocab)?;
                Ok(rows.pop().unwrap())
            }
        }
    }
}

/// candidates per row the host keeps when asked to reduce logits (the
/// "topk" verb); 256 comfortably covers the sampler's top_k <= 256 bound
const HOST_TOPK: i32 = 256;

fn topk_input() -> (String, Tensor) {
    ("topk".to_string(), Tensor::new(&[1], TensorType::I32, &HOST_TOPK.to_le_bytes()))
}

/// Per-request wasi-nn verb timing: every compute call asks the host for its
/// wall time ({"timing":1} -> "elapsed_us"), accumulated per verb label and
/// reported in the /chat done frame as "verb_us". Hosts that predate the
/// verb return nothing and the frame omits the block. One instance serves
/// one request, so a plain thread_local needs no reset discipline.
thread_local! {
    static VERB_TIMING: std::cell::RefCell<std::collections::BTreeMap<&'static str, (u64, u64)>> =
        std::cell::RefCell::new(std::collections::BTreeMap::new());
}

fn timing_input() -> (String, Tensor) {
    ("timing".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes()))
}

fn note_timing(label: &'static str, outs: &[(String, Tensor)]) {
    if let Some(t) = outs.iter().find(|(n, _)| n == "elapsed_us") {
        let d = t.1.data();
        if d.len() >= 4 {
            let us = i32::from_le_bytes([d[0], d[1], d[2], d[3]]).max(0) as u64;
            VERB_TIMING.with(|m| {
                let mut m = m.borrow_mut();
                let e = m.entry(label).or_insert((0, 0));
                e.0 += 1;
                e.1 += us;
            });
        }
    }
}

fn timing_snapshot() -> Vec<(String, u64, u64)> {
    VERB_TIMING.with(|m| m.borrow().iter().map(|(k, v)| (k.to_string(), v.0, v.1)).collect())
}

/// Parse a compute()'s logits outputs into Rows: sparse "topk_ids"/
/// "topk_logits" when the host knows the topk verb, dense "logits" from
/// hosts that predate it (the request input is simply ignored there).
fn rows_from_outs(
    outs: &[(String, Tensor)],
    n_rows: usize,
    vocab: usize,
) -> Result<Vec<Row>, String> {
    if let (Some(ti), Some(tv)) = (
        outs.iter().find(|(n, _)| n == "topk_ids"),
        outs.iter().find(|(n, _)| n == "topk_logits"),
    ) {
        let idata = ti.1.data();
        let vdata = tv.1.data();
        if n_rows == 0 || idata.len() != vdata.len() || idata.len() % (n_rows * 4) != 0 {
            return Err("host returned malformed topk rows".into());
        }
        let per = idata.len() / n_rows;
        let mut rows = Vec::with_capacity(n_rows);
        for r in 0..n_rows {
            let ids: Vec<u32> = idata[r * per..(r + 1) * per]
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u32)
                .collect();
            let vals: Vec<f32> = vdata[r * per..(r + 1) * per]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            rows.push(Row { ids: Some(ids), vals });
        }
        return Ok(rows);
    }
    let logits = outs
        .iter()
        .find(|(n, _)| n == "logits")
        .ok_or("ggml backend returned no \"logits\" output")?;
    let data = logits.1.data();
    let row = vocab * 4;
    if data.len() != row * n_rows {
        return Err(format!(
            "expected {} logit rows of {} bytes, got {} bytes - wrong model_volume \
             for this config, or the host predates per-position logits?",
            n_rows, row, data.len()
        ));
    }
    Ok(data
        .chunks_exact(row)
        .map(|r| {
            Row::dense(
                r.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        })
        .collect())
}

impl Session {
    /// ggml only: hand ONE image to the host, which encodes it and splices the
    /// result into this sequence at the current position. Returns the POSITIONS
    /// it consumed - not a token count, and not something the guest can derive:
    /// a dynamic-resolution model prices an image by its own grid, and M-RoPE
    /// numbers image positions differently again.
    ///
    /// The bytes cross as the file, not as pixels. Decoding, resizing, the
    /// vision encoder and the model's marker tokens all live behind the host's
    /// projector, so this app carries no image code at all.
    fn feed_image(&mut self, bytes: &[u8]) -> Result<usize, String> {
        let Session::Ggml { ctx } = self else {
            return Err("vision needs the ggml backend".into());
        };
        let outs = ctx
            .compute(vec![(
                "image".to_string(),
                Tensor::new(&[bytes.len() as u32], TensorType::U8, bytes),
            )])
            .map_err(|e| {
                let e = nn_err("image", e);
                // A host that KNOWS the verb tags what went wrong with one of
                // its own markers. Anything else came from a host that does
                // not know it at all - and what such a host says ("missing
                // \"tokens\" input", "unknown input") is not a sentence anyone
                // can act on, so name the real condition instead.
                const KNOWN: &[&str] = &[
                    "[image_undecodable]", "[image_too_wide]", "[vision_unavailable]",
                    "[kv_pool_full]",
                ];
                if KNOWN.iter().any(|m| e.contains(m)) {
                    e
                } else {
                    format!(
                        "[vision_unsupported] this deployment's node cannot read images: its \
                         llama.cpp toolchain predates vision support, so the model never got \
                         the picture (host said: {e})"
                    )
                }
            })?;
        let n = outs
            .iter()
            .find(|(n, _)| n == "image_pos")
            .ok_or("[vision_unsupported] host returned no \"image_pos\" output")?;
        let data = n.1.data();
        if data.len() < 4 {
            return Err("host returned a malformed image_pos".into());
        }
        let pos = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if pos <= 0 {
            return Err("the model consumed no positions for this image".into());
        }
        Ok(pos as usize)
    }

    /// ggml only: feed `ids` and get EVERY position's logits row back
    /// (dims [n, vocab]) - the speculative verify pass: the target consumes
    /// the draft's proposals in ONE forward pass.
    fn feed_all(&mut self, cfg: &AppConfig, ids: &[u32]) -> Result<Vec<Row>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("all".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
                topk_input(),
                timing_input(),
            ])
            .map_err(|e| nn_err("compute", e))?;
        note_timing("feed_all", &outs);
        rows_from_outs(&outs, ids.len(), cfg.vocab)
    }

    /// ggml only: this session's capabilities - (seq_id, recurrent, mtp,
    /// vision). seq_id is the handle another session on the SAME graph names
    /// to branch from it (`copy_from`); mtp = the loaded GGUF carries a
    /// multi-token-prediction head; vision = the volume carries a projector
    /// AND the node can drive it. Errors on hosts that predate the
    /// speculative toolchain, which is the capability probe: no caps, no
    /// speculative decode. The list has only grown, so an older host simply
    /// returns fewer values and each missing one reads as "no".
    fn caps(&mut self) -> Result<Caps, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let outs = ctx
            .compute(vec![(
                "caps".to_string(),
                Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes()),
            )])
            .map_err(|e| nn_err("caps", e))?;
        let caps = outs
            .iter()
            .find(|(n, _)| n == "caps")
            .ok_or("host returned no \"caps\" output")?;
        let data = caps.1.data();
        if data.len() < 8 {
            return Err("caps output too short".into());
        }
        let v = |i: usize| {
            i32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]])
        };
        Ok(Caps {
            seq: v(0),
            recurrent: v(1) != 0,
            mtp: data.len() >= 12 && v(2) != 0,
            vision: data.len() >= 16 && v(3) != 0,
        })
    }

    /// ggml only: MTP-aware feed of this sequence - the host runs an
    /// all-positions pass, mirrors every position into the model's own MTP
    /// head, and returns only the LAST logits row. The speculative prefill.
    fn feed_mtp(&mut self, cfg: &AppConfig, ids: &[u32]) -> Result<Row, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("mtp".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
                topk_input(),
                timing_input(),
            ])
            .map_err(|e| nn_err("compute", e))?;
        note_timing("feed_mtp", &outs);
        let mut rows = rows_from_outs(&outs, 1, cfg.vocab)?;
        Ok(rows.pop().unwrap())
    }

    /// ggml only: verify pass that ALSO harvests the target's MTP hidden
    /// rows for `real_seq` (the verify runs on a scratch branch, but the
    /// head state belongs to the real sequence).
    fn feed_all_mtp(
        &mut self,
        cfg: &AppConfig,
        ids: &[u32],
        real_seq: i32,
    ) -> Result<Vec<Row>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("all".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
                ("mtp_for".to_string(), Tensor::new(&[1], TensorType::I32, &real_seq.to_le_bytes())),
                topk_input(),
                timing_input(),
            ])
            .map_err(|e| nn_err("compute", e))?;
        note_timing("feed_all_mtp", &outs);
        rows_from_outs(&outs, ids.len(), cfg.vocab)
    }

    /// ggml only: the model's own MTP head proposes up to k tokens (greedy,
    /// stops when its confidence drops below p_min). May return fewer or none.
    fn mtp_draft(&mut self, id_last: u32, k: usize, p_min_milli: i32) -> Result<Vec<u32>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(12);
        bytes.extend_from_slice(&(id_last as i32).to_le_bytes());
        bytes.extend_from_slice(&(k as i32).to_le_bytes());
        bytes.extend_from_slice(&p_min_milli.to_le_bytes());
        let outs = ctx
            .compute(vec![
                ("mtp_draft".to_string(), Tensor::new(&[3], TensorType::I32, &bytes)),
                timing_input(),
            ])
            .map_err(|e| nn_err("mtp_draft", e))?;
        note_timing("mtp_draft", &outs);
        let draft = outs
            .iter()
            .find(|(n, _)| n == "draft")
            .ok_or("host returned no \"draft\" output")?;
        Ok(draft
            .1
            .data()
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u32)
            .collect())
    }

    /// ggml only: mirror a verify round's accepted tokens into the MTP head
    /// (pairs them with the rows harvested by the last mtp-flagged pass).
    fn mtp_accept(&mut self, pos0: usize, tokens: &[u32]) -> Result<(), String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(4 + tokens.len() * 4);
        bytes.extend_from_slice(&(pos0 as i32).to_le_bytes());
        for &t in tokens {
            bytes.extend_from_slice(&(t as i32).to_le_bytes());
        }
        let outs = ctx
            .compute(vec![
                ("mtp_accept".to_string(), Tensor::new(&[(1 + tokens.len()) as u32], TensorType::I32, &bytes)),
                timing_input(),
            ])
            .map_err(|e| nn_err("mtp_accept", e))?;
        note_timing("mtp_accept", &outs);
        Ok(())
    }

    /// ggml only: make THIS session an exact branch of `src_seq` (a sibling
    /// session on the same graph, at `src_fed` fed tokens). Attention KV is
    /// shared cells; recurrent state is copy-on-write - branching is free
    /// until this branch decodes. The speculative primitive that replaces
    /// rewind: verify draft tokens on a branch, adopt the branch on full
    /// accept, re-feed only the accepted tokens on partial accept.
    fn copy_from(&mut self, src_seq: i32, src_fed: usize) -> Result<(), String> {
        let Session::Ggml { ctx } = self else {
            return Err("copy_from needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&src_seq.to_le_bytes());
        bytes.extend_from_slice(&(src_fed as i32).to_le_bytes());
        let outs = ctx
            .compute(vec![
                ("copy_from".to_string(), Tensor::new(&[2], TensorType::I32, &bytes)),
                timing_input(),
            ])
            .map_err(|e| nn_err("copy_from", e))?;
        note_timing("copy_from", &outs);
        Ok(())
    }
}

/// Vet a request's attachments against the model and the deployment's limits,
/// BEFORE any of the expensive machinery starts. Returns how many images the
/// turn carries.
///
/// Everything here fails with a sentence naming the thing to change, because
/// every one of these is a mistake someone can only fix if they know which of
/// four different reasons stopped them: this model cannot see, this backend
/// cannot see, too many pictures, or one picture too large.
fn check_images(
    raw: &serde_json::Value,
    cfg: &AppConfig,
    messages: &[ChatMsg],
) -> Result<usize, String> {
    let n: usize = messages.iter().map(|m| m.images.len()).sum();
    if n == 0 {
        return Ok(0);
    }
    // A configured vision service reads for EVERY model, so the serving
    // model's own inability is no longer a reason to refuse the turn - the
    // picture goes to the sibling deployment instead (see apply_vision).
    if (!cfg.vision || cfg.backend != "ggml") && cfg.vision_service.is_none() {
        let others: Vec<String> = available_models(raw)
            .iter()
            .filter(|e| e.cfg.vision && e.cfg.backend == "ggml")
            .map(|e| e.cfg.name.clone())
            .collect();
        return Err(format!(
            "[no_vision] {} cannot read images{}",
            cfg.name,
            if others.is_empty() {
                ". No model attached to this deployment can; attach a vision model volume \
                 (weights plus an mmproj projector) and add it to the config's catalog, or \
                 point the config's vision_service at an image-reader deployment and every \
                 model here gains the capability"
                    .to_string()
            } else {
                format!(", but this deployment also serves {} - select it and resend", others.join(", "))
            }
        ));
    }
    if n > cfg.max_images {
        return Err(format!(
            "[too_many_images] {n} images in one request; this deployment allows {} \
             (each one costs about {} tokens of the context window)",
            cfg.max_images, cfg.image_tokens
        ));
    }
    if let Some(big) = messages
        .iter()
        .flat_map(|m| m.images.iter())
        .find(|b| b.len() > cfg.max_image_bytes)
    {
        return Err(format!(
            "[image_too_large] an image is {} KB; the limit is {} KB - resize it before \
             sending (the playground does this in the browser)",
            big.len() / 1024,
            cfg.max_image_bytes / 1024
        ));
    }
    Ok(n)
}

/// What the host says this session can do (see Session::caps).
struct Caps {
    seq: i32,
    #[allow(dead_code)] // read through the tuple destructuring in open_spec
    recurrent: bool,
    mtp: bool,
    vision: bool,
}

/// Everything speculative decoding needs beyond the target session: the
/// draft session plus one scratch branch per model, with the sequence ids
/// to branch from / adopt into. Opened non-retrying: a busy slot or a host
/// without the speculative verbs degrades to plain decode - drafting is an
/// accelerator, never a dependency.
struct SpecRig {
    dsess: Session,
    tscr: Session,
    dscr: Session,
    t_seq: i32,
    d_seq: i32,
    tscr_seq: i32,
    dscr_seq: i32,
}

fn open_spec(
    cfg: &AppConfig,
    dcfg: &AppConfig,
    target: ExecutionTarget,
    sess: &mut Session,
) -> Result<SpecRig, String> {
    let t_seq = sess.caps()?.seq; // also the host capability probe
    let mut dsess = Session::open(dcfg, target)?;
    let d_seq = dsess.caps()?.seq;
    let mut tscr = Session::open(cfg, target)?;
    let tscr_seq = tscr.caps()?.seq;
    let mut dscr = Session::open(dcfg, target)?;
    let dscr_seq = dscr.caps()?.seq;
    Ok(SpecRig { dsess, tscr, dscr, t_seq, d_seq, tscr_seq, dscr_seq })
}

/// The MTP rig: just the target scratch branch - the model drafts for
/// itself through the host's head context, so no draft model, no draft
/// sessions, half the slot cost of a model-draft rig.
struct MtpRig {
    tscr: Session,
    t_seq: i32,
    tscr_seq: i32,
}

fn open_mtp(
    cfg: &AppConfig,
    target: ExecutionTarget,
    sess: &mut Session,
) -> Result<MtpRig, String> {
    let caps = sess.caps()?; // also the host capability probe
    if !caps.mtp {
        return Err("this model volume carries no MTP head (use an *-mtp volume, or name a draft model)".into());
    }
    let t_seq = caps.seq;
    let mut tscr = Session::open(cfg, target)?;
    let tscr_seq = tscr.caps()?.seq;
    Ok(MtpRig { tscr, t_seq, tscr_seq })
}

/// Prompt-lookup uses the same rig shape as MTP (target + one scratch
/// branch) but needs no head - only the branch-verify verbs, so any ggml
/// model qualifies.
fn open_lookup(
    cfg: &AppConfig,
    target: ExecutionTarget,
    sess: &mut Session,
) -> Result<MtpRig, String> {
    let t_seq = sess.caps()?.seq; // also the host capability probe
    let mut tscr = Session::open(cfg, target)?;
    let tscr_seq = tscr.caps()?.seq;
    Ok(MtpRig { tscr, t_seq, tscr_seq })
}

struct GenParams {
    max_new: usize,
    sample: SampleParams,
    stop_strings: Vec<String>,
    /// tokens the reply may spend inside its <think> block before the block
    /// is forced shut (0 = uncapped).
    think_budget: usize,
    /// did the prompt force-open a <think> block? Tracked SEPARATELY from the
    /// budget, because "no cap on the block" and "there is no block" are not
    /// the same thing: the reply-is-looping rescue below has to know it is
    /// inside a reasoning block even when nothing caps it.
    think_open: bool,
    /// identical consecutive token blocks that end a degenerate reply
    /// (0 = off). Unlike think_budget this applies to the whole reply.
    loop_reps: usize,
}

struct GenStats {
    target: String,
    prompt_tokens: usize,
    tokens: usize,
    load_ms: u128,
    prefill_ms: u128,
    decode_ms: u128,
    finish_reason: &'static str,
    text: String,
    /// speculative decoding counters (0/0 on the plain path): how many
    /// tokens the draft proposed and how many the target accepted
    drafted: usize,
    accepted: usize,
    /// the think budget ran out and the server closed the block (see ThinkGuard)
    think_forced: bool,
    /// images the host encoded into this prompt, and the positions they
    /// actually cost (0/0 on a text turn). The positions are the host's own
    /// figure, not the config's budget estimate, so a deployment can see what
    /// its window is really being spent on.
    images: usize,
    image_pos: usize,
}


/// The incremental text pipeline shared by the plain and speculative decode
/// loops: push sampled tokens one at a time; it detokenizes the sequence,
/// holds back the longest stop string so one is never partially emitted,
/// sends stable deltas, and reports a stop-string hit.
enum Pushed {
    More,        // keep generating
    Stopped,     // a stop string landed; text is truncated at the match
    Gone,        // client disconnected mid-emit
}
/// The tokenizer's incremental decoder: one per generation. Replaces the
/// old whole-history re-decode (tok.decode(&generated) EVERY token - O(n^2)
/// over a reply, and a full-text stop-string scan on top). The stream hands
/// back each token's text the moment its bytes complete a character;
/// partial UTF-8 is withheld by the stream itself, so the old U+FFFD tail
/// check is gone with the quadratic work.
type TokStream<'a> = tokenizers::tokenizer::DecodeStream<
    'a,
    tokenizers::ModelWrapper,
    tokenizers::NormalizerWrapper,
    tokenizers::PreTokenizerWrapper,
    tokenizers::PostProcessorWrapper,
    tokenizers::DecoderWrapper,
>;

struct TextOut<'a> {
    stream: TokStream<'a>,
    emit: &'a dyn Fn(&str) -> bool,
    stops: &'a [String],
    holdback: usize,
    generated: Vec<u32>,
    emitted: usize,
    text: String,
}
impl<'a> TextOut<'a> {
    fn new(tok: &'a Tokenizer, emit: &'a dyn Fn(&str) -> bool, stops: &'a [String]) -> TextOut<'a> {
        TextOut {
            stream: tok.decode_stream(true), emit, stops,
            holdback: stops.iter().map(|s| s.len()).max().unwrap_or(0),
            generated: Vec::new(), emitted: 0, text: String::new(),
        }
    }
    fn push(&mut self, next: u32) -> Pushed {
        self.generated.push(next);
        let delta = match self.stream.step(next) {
            Ok(Some(d)) if !d.is_empty() => d,
            _ => return Pushed::More, // withheld partial char, special token, or a decoder hiccup
        };
        // a stop string can only COMPLETE inside (old holdback tail + delta):
        // scan that window, not the whole reply (backed off to a char boundary)
        let mut sf = self.text.len().saturating_sub(self.holdback);
        self.text.push_str(&delta);
        while sf > 0 && !self.text.is_char_boundary(sf) {
            sf -= 1;
        }
        if let Some(pos) = self
            .stops
            .iter()
            .filter_map(|s| self.text[sf..].find(s.as_str()).map(|p| p + sf))
            .min()
        {
            self.text.truncate(pos);
            if pos > self.emitted {
                if !(self.emit)(&self.text[self.emitted..pos]) { return Pushed::Gone; }
                self.emitted = pos;
            }
            return Pushed::Stopped;
        }
        let visible = self.text.len().saturating_sub(self.holdback);
        if visible > self.emitted {
            if let Some(delta) = self.text.get(self.emitted..visible) {
                if !(self.emit)(delta) { return Pushed::Gone; }
                self.emitted = visible;
            }
        }
        Pushed::More
    }
    /// send whatever the holdback was withholding (end of generation)
    fn flush(&mut self) {
        if self.text.len() > self.emitted {
            if let Some(delta) = self.text.get(self.emitted..) {
                let _ = (self.emit)(delta);
            }
        }
    }
    /// Push a run of tokens the SERVER chose rather than the model (the
    /// think-budget close). They are ordinary tokens in every other respect -
    /// visible, counted, part of the rep-penalty window. Stop strings cannot
    /// occur inside a run this short and fixed, so the only outcome worth
    /// reporting is a client disconnect: false = gone.
    fn push_forced(&mut self, ids: &[u32]) -> bool {
        for &t in ids {
            if matches!(self.push(t), Pushed::Gone) {
                return false;
            }
        }
        true
    }
    /// the repetition-penalty window: the freshest `w` sampled tokens,
    /// falling back to the prompt tail before anything is generated
    fn recent(&self, prompt_ids: &[u32], w: usize) -> Vec<u32> {
        if self.generated.is_empty() {
            prompt_ids[prompt_ids.len().saturating_sub(w)..].to_vec()
        } else {
            self.generated[self.generated.len().saturating_sub(w)..].to_vec()
        }
    }
}

const THINK_CLOSE: &str = "</think>";
/// What the server writes into the reply to force the block shut. The leading
/// newline lands the tag on its own line even when the budget cut the model
/// off mid-word; the trailing pair matches how the qwen3.x templates end a
/// think block, so the answer starts exactly where the model expects it to.
const THINK_CLOSE_TEXT: &str = "\n</think>\n\n";

/// The think-budget watchdog: it keeps a reasoning model from spending a
/// whole reply inside <think>. Left alone, a model that loops in the block -
/// re-deriving the same step, second-guessing the same answer - runs to
/// max_new and the user gets a wall of reasoning and no answer at all.
///
/// At the budget the block is FORCE-CLOSED: THINK_CLOSE_TEXT is pushed into
/// the visible reply and fed to the model as real tokens on the real
/// sequence, so the model's own context says the reasoning is over and it
/// writes the answer with what it has. Every decode path does the feed its
/// own way (plain, speculative, MTP) but they all inject the same tokens at
/// the same point: right after a sampled token is pushed and before it is
/// fed, where the sequence is in a known state.
///
/// The block is open from the very first generated token whenever the prompt
/// force-opened it (ThinkTurn::Open - the only way these templates start
/// one), so the in-block count IS out.generated.len(). A model that opens a
/// block by itself mid-reply is not tracked; nothing in the qwen3.x family
/// does that, because the template already wrote the opening tag.
/// Stops a reply that has collapsed into a loop.
///
/// This exists because ThinkGuard does NOT cover the case: it only arms on a
/// turn whose prompt force-opened a `<think>` block, so a model that reasons
/// in ordinary prose - "Thinking Process:" as literal text, which is exactly
/// what fable-fusion does with thinking off - has no guard at all. Observed
/// live 2026-07-29 on "write a haiku about secure enclaves": the reply
/// repeated one seven-token phrase until it hit max_new_cap, 80k tokens of
/// the same line.
///
/// Sampling settings reduce the odds (top_k especially) but cannot promise
/// anything: a degenerate loop is always reachable, so the decoder needs a
/// hard stop and not just better odds.
///
/// Detection is exact-block, not fuzzy: the tail must be one identical run of
/// tokens repeated N times. That will not catch a drifting near-repeat, but
/// it also cannot fire on prose that merely rhymes or a list with a shared
/// prefix - and a false stop truncates someone's real answer, which is worse
/// than a loop that runs a little longer before tripping.
struct LoopGuard {
    /// identical consecutive blocks that end the reply; 0 = off
    reps: usize,
    /// longest repeating unit considered, in tokens
    max_period: usize,
}

impl LoopGuard {
    fn new(reps: usize) -> LoopGuard {
        LoopGuard { reps, max_period: 64 }
    }

    /// Short blocks need more evidence: a couple of repeated newlines or a
    /// run of dashes in a table is ordinary text, while the same twelve
    /// tokens four times over is not.
    fn required(&self, period: usize) -> usize {
        match period {
            1 => self.reps * 6,
            2..=4 => self.reps * 3,
            _ => self.reps,
        }
    }

    fn tripped(&self, g: &[u32]) -> bool {
        if self.reps == 0 {
            return false;
        }
        for period in 1..=self.max_period {
            let need = self.required(period);
            let span = period * need;
            // NOT `break`: required() demands extra repeats of short blocks,
            // so span does not grow monotonically with period (period 3 wants
            // 36 tokens, period 7 only 28). Bailing at the first period that
            // does not fit skipped every longer one - which is to say, the
            // whole class of failure this guard exists for.
            if g.len() < span {
                continue;
            }
            let tail = &g[g.len() - span..];
            let first = &tail[..period];
            if tail.chunks(period).all(|c| c == first) {
                return true;
            }
        }
        false
    }
}

struct ThinkGuard {
    budget: usize, // 0 = uncapped; the block is still TRACKED
    open: bool,
    scanned: usize, // bytes of out.text already searched for the closing tag
    close: Vec<u32>,
    forced: bool,
    /// close on the next check whatever the budget says (the loop rescue)
    force_now: bool,
    /// the rescue has been spent; a reply only gets one
    by_loop: bool,
}

impl ThinkGuard {
    fn new(budget: usize, think_open: bool, tok: &Tokenizer) -> ThinkGuard {
        // a tokenizer that cannot produce the closing tag leaves the guard
        // off: a close we cannot write is better than a mangled reply
        let close = if think_open {
            tok.encode(THINK_CLOSE_TEXT, false)
                .map(|e| e.get_ids().to_vec())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        ThinkGuard {
            budget,
            open: !close.is_empty(),
            scanned: 0,
            close,
            forced: false,
            force_now: false,
            by_loop: false,
        }
    }

    /// The reply has collapsed into a loop. If that is happening INSIDE the
    /// reasoning block then the BLOCK is what is stuck, and ending the reply
    /// there is how a user gets a page of thinking and no answer at all - the
    /// worst outcome available, since the model had not yet written a word
    /// meant for them. So the block is force-closed instead and the answer
    /// generates outside it, exactly as when the budget runs out.
    ///
    /// Once per reply. A model that loops AGAIN after the block is shut is
    /// genuinely degenerate, and then stopping is right.
    fn take_loop(&mut self) -> bool {
        if !self.open || self.by_loop {
            return false;
        }
        self.by_loop = true;
        self.force_now = true;
        true
    }

    /// Called after each pushed token: is the budget spent with the block
    /// still open? The guard falls silent for the rest of the reply either
    /// way - once the model closes the block on its own, or once we have.
    fn over(&mut self, out: &TextOut) -> bool {
        if !self.open {
            return false;
        }
        // the tag can straddle two pushes, so re-read its own length of
        // already-scanned text; incremental detokenization can also rewrite
        // the tail (a replacement char completing into a shorter char), hence
        // the clamp and the walk back to a char boundary
        let mut from = self.scanned.saturating_sub(THINK_CLOSE.len()).min(out.text.len());
        while from > 0 && !out.text.is_char_boundary(from) {
            from -= 1;
        }
        if out.text[from..].contains(THINK_CLOSE) {
            self.open = false;
            return false;
        }
        self.scanned = out.text.len();
        // budget 0 is UNCAPPED, not "close immediately": only a rescue closes
        // a block that nothing caps
        if !self.force_now && (self.budget == 0 || out.generated.len() < self.budget) {
            return false;
        }
        self.open = false;
        self.forced = true;
        true
    }

    fn note(&self) -> String {
        if self.by_loop {
            return "the reasoning block was repeating itself - closing it so the answer can \
                    start, instead of ending the reply with nothing but thinking in it"
                .to_string();
        }
        format!(
            "the think budget of {} tokens is spent and the reasoning block is still open - \
             closing it so the answer can start",
            self.budget
        )
    }
}

/// Run the full completion; `emit` receives text deltas as they stabilize,
/// `status` receives progress lines. Both return false when the client is
/// gone. Status events double as keepalive bytes during the one long silence
/// (cold session init; the host caches sessions). With `draft` set (a
/// same-tokenizer catalog sibling), decoding is SPECULATIVE: the draft
/// proposes draft_tokens ahead, the target verifies them in one pass, and
/// every accepted token skips a full target step; any draft-side failure
/// falls back to plain decode with a status note.
fn generate(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt: &Prompt,
    target: ExecutionTarget,
    tname: &str,
    p: &GenParams,
    draft: &DraftPlan,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    // The repetition window and the RNG seed read the text side of the
    // prompt; images contribute no token ids to look back at.
    let prompt_ids: &[u32] = &prompt.text_ids;
    if !status(&format!(
        "loading the model on {tname} - the first request after a node boot initializes the session and can take a while"
    )) {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    // The host caps live inference sessions (each pre-allocates the full
    // context window's KV in the share's VRAM) and fails fast when they are
    // all taken - queue politely: status keepalives out, poll every couple of
    // seconds, give up only after a real wait or when the client hangs up.
    let mut sess = loop {
        match Session::open(cfg, target) {
            Ok(s) => break s,
            Err(e) if e.contains(BUSY_MARKER) => {
                if now_ms() - t0 > BUSY_WAIT_BUDGET_MS {
                    return Err(format!(
                        "every inference session stayed busy for {}s - this deployment is at \
                         its concurrent-chat capacity right now; try again in a little while",
                        BUSY_WAIT_BUDGET_MS / 1000
                    ));
                }
                if !status(&format!(
                    "{BUSY_STATUS} ({}s) - waiting for a free slot",
                    (now_ms() - t0) / 1000
                )) {
                    return Err("client disconnected".into());
                }
                sleep_ms(BUSY_POLL_MS);
            }
            Err(e) => return Err(e),
        }
    };
    let load_ms = now_ms() - t0;

    // speculative path: the rig opening non-retrying - busy slots, an unfit
    // share, a headless volume, or a host predating the speculative verbs
    // all quietly downgrade to plain decode. Drafting is an accelerator,
    // never a dependency.
    // The config said this model can see; ask the HOST, which is the half that
    // actually has to (the volume's projector, and a node new enough to drive
    // it). Doing it here, before the prefill, means a deployment whose node
    // predates vision fails in a sentence instead of after a minute of encode
    // work. A host too old to answer `caps` at all is not judged: the image
    // feed itself will say so, and that path already has the better message.
    if prompt.images > 0 {
        if let Ok(caps) = sess.caps() {
            if !caps.vision {
                return Err(format!(
                    "[no_vision] the \"{}\" volume on this node cannot read images: the \
                     model is configured for vision, but the node reports no projector \
                     (is the mmproj gguf in the volume, and is this node's llama.cpp \
                     toolchain new enough?)",
                    cfg.model_volume
                ));
            }
        }
    }

    // Speculative decode counts POSITIONS in tokens (a branch is adopted at a
    // token offset, a partial accept re-feeds a token count). An image does not
    // occupy one position per token, so a prompt carrying one takes the plain
    // path - correctness first, and the drafting was only ever an accelerator.
    match (draft, prompt.text_only()) {
        (DraftPlan::Model(dc), Some(ids)) => match open_spec(cfg, dc, target, &mut sess) {
            Ok(rig) => {
                if !status(&format!("session ready ({load_ms} ms); speculative decode via {} - prefilling {} prompt tokens", dc.name, ids.len())) {
                    return Err("client disconnected".into());
                }
                return generate_spec(cfg, dc, tok, ids, tname, p, sess, rig, load_ms, emit, status);
            }
            Err(e) => {
                let _ = status(&format!("draft model unavailable ({}); plain decode", strip_code(&e)));
            }
        },
        (DraftPlan::Mtp, Some(ids)) => match open_mtp(cfg, target, &mut sess) {
            Ok(rig) => {
                if !status(&format!("session ready ({load_ms} ms); speculative decode via the model's MTP head - prefilling {} prompt tokens", ids.len())) {
                    return Err("client disconnected".into());
                }
                return generate_mtp(cfg, tok, ids, tname, p, sess, rig, load_ms, emit, status);
            }
            Err(e) => {
                let _ = status(&format!("MTP drafting unavailable ({}); plain decode", strip_code(&e)));
            }
        },
        (DraftPlan::Lookup, Some(ids)) => match open_lookup(cfg, target, &mut sess) {
            Ok(rig) => {
                if !status(&format!("session ready ({load_ms} ms); speculative decode via prompt lookup - prefilling {} prompt tokens", ids.len())) {
                    return Err("client disconnected".into());
                }
                return generate_lookup(cfg, tok, ids, tname, p, sess, rig, load_ms, emit, status);
            }
            Err(e) => {
                let _ = status(&format!("prompt-lookup drafting unavailable ({}); plain decode", strip_code(&e)));
            }
        },
        (DraftPlan::Model(_) | DraftPlan::Mtp | DraftPlan::Lookup, None) => {
            let _ = status("speculative decode off for this turn: the prompt carries an image");
        }
        (DraftPlan::Plain, _) => {}
    }
    if !status(&format!(
        "session ready ({load_ms} ms); prefilling {} prompt tokens{}",
        prompt_ids.len(),
        match prompt.images {
            0 => String::new(),
            1 => " and 1 image".into(),
            n => format!(" and {n} images"),
        }
    )) {
        return Err("client disconnected".into());
    }

    // -- prefill. Text goes in chunks so no single logits tensor gets huge;
    // an image goes whole, to the host, which encodes it and splices the
    // result into the sequence at the position the text left off.
    let t1 = now_ms();
    let mut logits = Row::dense(Vec::new());
    let mut image_pos = 0usize;
    let last_part = prompt.parts.len().saturating_sub(1);
    for (i, part) in prompt.parts.iter().enumerate() {
        match part {
            PromptPart::Text(ids) => {
                let mut done = 0usize;
                while done < ids.len() {
                    let end = (done + PREFILL_CHUNK).min(ids.len());
                    // only the very last token of the whole prompt needs logits
                    let last = i == last_part && end == ids.len();
                    let l = sess.feed(cfg, &ids[done..end], last)?;
                    if last {
                        logits = l;
                    }
                    done = end;
                }
            }
            PromptPart::Image(bytes) => {
                if !status(&format!(
                    "reading the image ({} KB) - the vision encoder runs on the same share as the model",
                    bytes.len() / 1024
                )) {
                    return Err("client disconnected".into());
                }
                image_pos += sess.feed_image(bytes)?;
            }
        }
    }
    if logits.vals.is_empty() {
        // A prompt whose last part is an image: nothing sampled a row yet. The
        // templates all end an assistant turn's opening with text, so this
        // means the render went wrong rather than the model.
        return Err("prompt ended without a text turn to answer from".into());
    }
    let prefill_ms = now_ms() - t1;

    // -- decode
    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut think = ThinkGuard::new(p.think_budget, p.think_open, tok);
    let loop_guard = LoopGuard::new(p.loop_reps);
    // where the repetition check starts: moved past a force-closed reasoning
    // block so the answer is not condemned for the loop that preceded it
    let mut guard_from = 0usize;
    let mut finish: &'static str = "stop";
    loop {
        let recent = out.recent(prompt_ids, p.sample.rep_window);
        let next = pick_row(&mut logits, &recent, &p.sample, &mut rng);
        if cfg.eos.contains(&next) {
            break;
        }
        if out.generated.len() >= p.max_new {
            finish = "length";
            break;
        }
        match out.push(next) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted: 0, accepted: 0,
                    think_forced: think.forced, images: prompt.images, image_pos,
                });
            }
            Pushed::Gone => break, // client disconnected
        }
        // a reply that has collapsed into a loop is finished, whatever it
        // thinks it is doing - checked here so it covers plain prose, which
        // the think budget below never sees. Inside the reasoning block it is
        // the BLOCK that ends instead of the reply (take_loop), because a reply
        // that stops there has nothing in it the user asked for.
        if loop_guard.tripped(&out.generated[guard_from..]) && !think.take_loop() {
            finish = "repetition";
            break;
        }
        // budget spent (or the rescue above): close the block in the same pass
        // that feeds `next`, so the model's next sample is already outside it
        if think.over(&out) {
            let _ = status(&think.note());
            if !out.push_forced(&think.close) {
                break;
            }
            // the answer is judged for repetition on ITS OWN tokens: the loop
            // that just ended is still in `generated` and would trip the guard
            // again on the next token
            guard_from = out.generated.len();
            let mut ids = vec![next];
            ids.extend_from_slice(&think.close);
            logits = sess.feed(cfg, &ids, true)?;
            continue;
        }
        logits = sess.feed(cfg, &[next], true)?;
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted: 0, accepted: 0,
        think_forced: think.forced, images: prompt.images, image_pos,
    })
}

/// Speculative decode: EXACT-MATCH verification. Every emitted token is
/// sampled by the TARGET's own sampler on the target's logits (identical
/// distribution to plain decode - at temperature 0, identical tokens); the
/// draft only predicts what that sampler will say, and each correct
/// prediction folds a full target step into the shared verify pass. Per
/// round: the draft proposes k tokens one cheap step at a time, the target
/// consumes [pending, d1..dk] in ONE pass returning every position's row,
/// and acceptance walks the rows until the target's sample disagrees - the
/// disagreeing sample itself becomes the next token (nothing is wasted).
///
/// BRANCH-COMMIT, not rewind: proposals and the verify pass run on SCRATCH
/// branches (copy_from - shared attention cells, copy-on-write recurrent
/// state), never on the real sequences. A full accept ADOPTS the branch
/// (free); a partial accept re-feeds only the accepted tokens on the real
/// sequence (one batched pass) and abandons the branch. The real sequences
/// never contain a rejected token, so nothing ever needs removal - which is
/// what makes speculation work on hybrid-SSM models (qwen3.5/3.6), whose
/// recurrent state keeps no per-token history and cannot rewind. The
/// alternative (llama.cpp n_rs_seq state snapshots) would cost
/// n_seq_max * state_size per token of depth - ~1.2 GB per token on the
/// 27b at 8 slots - where branching costs zero.
#[allow(clippy::too_many_arguments)]
fn generate_spec(
    cfg: &AppConfig,
    dcfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    tname: &str,
    p: &GenParams,
    mut sess: Session,
    rig: SpecRig,
    load_ms: u128,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let SpecRig { mut dsess, mut tscr, mut dscr, t_seq, d_seq, tscr_seq, dscr_seq } = rig;
    let k = dcfg.draft_tokens.clamp(1, 16).min(cfg.draft_tokens.clamp(1, 16));
    let t1 = now_ms();
    // prefill BOTH models on the prompt; only the target's last row is needed
    let mut done = 0usize;
    let mut t_logits = Row::dense(Vec::new());
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let last = end == prompt_ids.len();
        let l = sess.feed(cfg, &prompt_ids[done..end], last)?;
        if last { t_logits = l; }
        done = end;
    }
    done = 0;
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        dsess.feed(dcfg, &prompt_ids[done..end], false)?;
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut think = ThinkGuard::new(p.think_budget, p.think_open, tok);
    let loop_guard = LoopGuard::new(p.loop_reps);
    let mut guard_from = 0usize; // see the plain loop
    let mut finish: &'static str = "stop";
    let (mut drafted, mut accepted) = (0usize, 0usize);
    // fed-token cursors, so rewinds land on absolute positions
    let mut t_fed = prompt_ids.len();
    let mut d_fed = prompt_ids.len();
    let mut d_behind: Vec<u32> = Vec::new(); // target-fed tokens the draft hasn't seen yet

    // the first token comes straight off the target's prefill row
    let recent = out.recent(prompt_ids, p.sample.rep_window);
    let mut pending = pick_row(&mut t_logits, &recent, &p.sample, &mut rng);
    'outer: loop {
        if cfg.eos.contains(&pending) { break; }
        if out.generated.len() >= p.max_new { finish = "length"; break; }
        match out.push(pending) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted, accepted,
                    think_forced: think.forced, images: 0, image_pos: 0,
                });
            }
            Pushed::Gone => break 'outer,
        }
        // -- degenerate loop: stop, unless it is the reasoning block that is
        //    stuck, in which case the block ends and the answer goes on (see
        //    the plain loop). Speculative decode makes this MORE likely to run
        //    long, not less - a looping target accepts its own drafts almost
        //    perfectly, so the repetition arrives faster.
        if loop_guard.tripped(&out.generated[guard_from..]) && !think.take_loop() {
            finish = "repetition";
            break 'outer;
        }
        // -- budget spent: force the block shut on the REAL target sequence
        //    (one plain pass, no speculation to unwind) and resample from the
        //    row it returns. The draft sees the whole run as ordinary history
        //    through next round's catchup, so both models stay in step.
        if think.over(&out) {
            let _ = status(&think.note());
            if !out.push_forced(&think.close) {
                break 'outer;
            }
            guard_from = out.generated.len();
            let mut ids = vec![pending];
            ids.extend_from_slice(&think.close);
            let mut row = sess.feed(cfg, &ids, true)?;
            t_fed += ids.len();
            d_behind.extend_from_slice(&ids);
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            pending = pick_row(&mut row, &recent, &p.sample, &mut rng);
            continue 'outer;
        }
        // -- catch the draft's REAL sequence up on accepted history (its
        //    branch only ever carries proposals), then branch it
        let mut catchup = std::mem::take(&mut d_behind);
        catchup.push(pending);
        let mut d_row = dsess.feed(dcfg, &catchup, true)?;
        d_fed += catchup.len();
        dscr.copy_from(d_seq, d_fed)?;
        let mut dscr_fed = d_fed;
        // -- k proposals, one cheap step each, on the draft BRANCH
        let mut drafts: Vec<u32> = Vec::with_capacity(k);
        for i in 0..k {
            let mut rec = out.recent(prompt_ids, p.sample.rep_window);
            rec.extend_from_slice(&drafts);
            let rec = rec[rec.len().saturating_sub(p.sample.rep_window)..].to_vec();
            let d = pick_row(&mut d_row, &rec, &p.sample, &mut rng);
            drafts.push(d);
            if i + 1 < k {
                d_row = dscr.feed(dcfg, &[d], true)?;
                dscr_fed += 1;
            }
        }
        drafted += drafts.len();
        // -- ONE verify pass over [pending, d1..dk] on the target BRANCH:
        //    k+1 rows, the real sequence untouched
        tscr.copy_from(t_seq, t_fed)?;
        let mut feed: Vec<u32> = Vec::with_capacity(k + 1);
        feed.push(pending);
        feed.extend_from_slice(&drafts);
        let mut rows = tscr.feed_all(cfg, &feed)?;
        let tscr_fed = t_fed + feed.len();
        // -- verify: accept while the target's own sample agrees. On every
        //    exit below the generation is OVER - the real sequences are
        //    simply dropped, so no cleanup of the branch is ever needed.
        let mut acc = 0usize;
        let mut replacement: Option<u32> = None;
        for (i, &d) in drafts.iter().enumerate() {
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let expect = pick_row(&mut rows[i], &recent, &p.sample, &mut rng);
            if expect != d {
                replacement = Some(expect);
                break;
            }
            // the draft predicted the target's sample exactly - it is the
            // target's token in every sense; run it through the same gates
            if cfg.eos.contains(&d) {
                accepted += acc;
                break 'outer;
            }
            if out.generated.len() >= p.max_new {
                finish = "length";
                accepted += acc;
                break 'outer;
            }
            match out.push(d) {
                Pushed::More => {}
                Pushed::Stopped => {
                    let decode_ms = now_ms() - t2;
                    return Ok(GenStats {
                        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                        finish_reason: "stop", text: out.text, drafted, accepted: accepted + acc + 1,
                        think_forced: think.forced, images: 0, image_pos: 0,
                    });
                }
                Pushed::Gone => { accepted += acc + 1; break 'outer; }
            }
            acc += 1;
        }
        accepted += acc;
        if let Some(r) = replacement {
            // partial round: commit ONLY the accepted tokens to the real
            // target (one batched pass, logits unread) and abandon both
            // branches - they are re-branched fresh next round. The draft's
            // real sequence gets the accepted drafts via next round's
            // catchup.
            sess.feed(cfg, &feed[..acc + 1], false)?;
            t_fed += acc + 1;
            d_behind = drafts[..acc].to_vec();
            pending = r;
        } else {
            // full acceptance: ADOPT both branches (attention cells shared,
            // recurrent state referenced - zero decode cost), bonus token
            // from the last row; the draft still owes itself d_k
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            dsess.copy_from(dscr_seq, dscr_fed)?;
            d_fed = dscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_row(&mut rows[last], &recent, &p.sample, &mut rng);
            d_behind.push(drafts[drafts.len() - 1]);
        }
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted, accepted,
        think_forced: think.forced, images: 0, image_pos: 0,
    })
}

/// MTP speculative decode: the branch-commit loop of generate_spec with the
/// model's OWN multi-token-prediction head as the proposer - no draft
/// sessions, no second model. Per round the host's head proposes up to k
/// tokens at near-zero cost (it rides hidden state harvested from prior
/// passes), the target verifies them on a scratch branch in one pass, and
/// only ACCEPTED tokens are mirrored back into the head, so its state stays
/// clean. Verification is EXACT-MATCH on the target's own sampler - output
/// is byte-for-byte plain decode, the head only changes speed.
#[allow(clippy::too_many_arguments)]
fn generate_mtp(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    tname: &str,
    p: &GenParams,
    mut sess: Session,
    rig: MtpRig,
    load_ms: u128,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let MtpRig { mut tscr, t_seq, tscr_seq } = rig;
    let k = cfg.draft_tokens.clamp(1, 16);
    let p_min_milli = (cfg.draft_p_min.clamp(0.0, 0.95) * 1000.0) as i32;
    // -- prefill through the MTP-aware feed: every chunk's positions are
    //    mirrored into the head, only last-row logits cross to the guest
    let t1 = now_ms();
    let mut done = 0usize;
    let mut t_logits = Row::dense(Vec::new());
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let l = sess.feed_mtp(cfg, &prompt_ids[done..end])?;
        if end == prompt_ids.len() { t_logits = l; }
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut think = ThinkGuard::new(p.think_budget, p.think_open, tok);
    let loop_guard = LoopGuard::new(p.loop_reps);
    let mut guard_from = 0usize; // see the plain loop
    let mut finish: &'static str = "stop";
    let (mut drafted, mut accepted) = (0usize, 0usize);
    let mut t_fed = prompt_ids.len();

    let recent = out.recent(prompt_ids, p.sample.rep_window);
    let mut pending = pick_row(&mut t_logits, &recent, &p.sample, &mut rng);
    'outer: loop {
        if cfg.eos.contains(&pending) { break; }
        if out.generated.len() >= p.max_new { finish = "length"; break; }
        match out.push(pending) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted, accepted,
                    think_forced: think.forced, images: 0, image_pos: 0,
                });
            }
            Pushed::Gone => break 'outer,
        }
        // -- degenerate loop: stop, or end just the reasoning block if that is
        //    what is stuck (see the plain loop; the MTP head predicts a
        //    repeating tail near-perfectly, so this path reaches the cap
        //    fastest of the three)
        if loop_guard.tripped(&out.generated[guard_from..]) && !think.take_loop() {
            finish = "repetition";
            break 'outer;
        }
        // -- budget spent: force the block shut. The close rides exactly the
        //    path a fully-accepted round takes - verified on the branch with
        //    head rows harvested for the real sequence, mirrored into the
        //    head, branch adopted - so the MTP head never goes stale on
        //    tokens the server chose rather than the model.
        if think.over(&out) {
            let _ = status(&think.note());
            if !out.push_forced(&think.close) {
                break 'outer;
            }
            guard_from = out.generated.len();
            let mut ids = vec![pending];
            ids.extend_from_slice(&think.close);
            tscr.copy_from(t_seq, t_fed)?;
            let mut rows = tscr.feed_all_mtp(cfg, &ids, t_seq)?;
            let tscr_fed = t_fed + ids.len();
            sess.mtp_accept(t_fed, &ids)?;
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_row(&mut rows[last], &recent, &p.sample, &mut rng);
            continue 'outer;
        }
        // -- the head proposes (0..=k tokens; an empty draft still verifies
        //    [pending] alone, which adopts as a plain step)
        let drafts = sess.mtp_draft(pending, k, p_min_milli)?;
        drafted += drafts.len();
        // -- ONE verify pass on the target branch, harvesting head rows for
        //    the REAL sequence
        tscr.copy_from(t_seq, t_fed)?;
        let mut feed: Vec<u32> = Vec::with_capacity(drafts.len() + 1);
        feed.push(pending);
        feed.extend_from_slice(&drafts);
        let mut rows = tscr.feed_all_mtp(cfg, &feed, t_seq)?;
        let tscr_fed = t_fed + feed.len();
        // -- verify: accept while the target's own sample agrees
        let mut acc = 0usize;
        let mut replacement: Option<u32> = None;
        for (i, &d) in drafts.iter().enumerate() {
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let expect = pick_row(&mut rows[i], &recent, &p.sample, &mut rng);
            if expect != d {
                replacement = Some(expect);
                break;
            }
            if cfg.eos.contains(&d) {
                accepted += acc;
                break 'outer;
            }
            if out.generated.len() >= p.max_new {
                finish = "length";
                accepted += acc;
                break 'outer;
            }
            match out.push(d) {
                Pushed::More => {}
                Pushed::Stopped => {
                    let decode_ms = now_ms() - t2;
                    return Ok(GenStats {
                        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                        finish_reason: "stop", text: out.text, drafted, accepted: accepted + acc + 1,
                        think_forced: think.forced, images: 0, image_pos: 0,
                    });
                }
                Pushed::Gone => { accepted += acc + 1; break 'outer; }
            }
            acc += 1;
        }
        accepted += acc;
        // -- the head learns ONLY the accepted tokens (its KV never holds a
        //    rejected proposal), then the target commits them
        sess.mtp_accept(t_fed, &feed[..acc + 1])?;
        if let Some(r) = replacement {
            sess.feed(cfg, &feed[..acc + 1], false)?;
            t_fed += acc + 1;
            pending = r;
        } else {
            // full acceptance (also the empty-draft case): adopt the branch
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_row(&mut rows[last], &recent, &p.sample, &mut rng);
        }
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted, accepted,
        think_forced: think.forced, images: 0, image_pos: 0,
    })
}

/// Prompt-lookup n-gram proposer: find the most recent PRIOR occurrence of
/// the sequence's last `ng` tokens and propose up to `k` tokens of what
/// followed it. The "model" is the conversation itself - names, phrases,
/// quoted code and structure recur constantly in chat - so proposals cost
/// nothing. A proposal may run into the current tail (self-overlapping
/// repetition), which correctly predicts a repeating pattern continuing.
fn lookup_propose(prompt: &[u32], gen: &[u32], ng: usize, k: usize) -> Vec<u32> {
    let total = prompt.len() + gen.len();
    if total < ng + 1 {
        return Vec::new();
    }
    let at = |i: usize| if i < prompt.len() { prompt[i] } else { gen[i - prompt.len()] };
    let key_start = total - ng;
    for i in (0..key_start).rev() {
        let mut m = true;
        for j in 0..ng {
            if at(i + j) != at(key_start + j) {
                m = false;
                break;
            }
        }
        if !m {
            continue;
        }
        let mut prop = Vec::with_capacity(k);
        let mut p = i + ng;
        while p < total && prop.len() < k {
            prop.push(at(p));
            p += 1;
        }
        if !prop.is_empty() {
            return prop;
        }
    }
    Vec::new()
}

/// tokens of trailing context that must match history before lookup proposes
/// (tried longest-first from LOOKUP_NGRAM_MAX down to LOOKUP_NGRAM). The
/// 2026-08-01 GPU matrix put 3-gram misfire acceptance at ~6-7%, pure round
/// tax - only well-anchored matches are worth a verify round.
const LOOKUP_NGRAM: usize = 5;
const LOOKUP_NGRAM_MAX: usize = 6;

/// Prompt-lookup speculative decode: the branch-commit loop with n-gram
/// matches against the conversation as the proposer - no draft model, no
/// MTP head, no host-side state beyond the branch-verify verbs, so it runs
/// on ANY ggml model. Rounds WITHOUT a match are literally plain decode
/// (one step on the real sequence, branch untouched): the scheme can only
/// spend the k extra verify rows on rounds where history actually matched,
/// which is what bounds its downside. Exact-match verification as always -
/// output is byte-for-byte the target's own.
#[allow(clippy::too_many_arguments)]
fn generate_lookup(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    tname: &str,
    p: &GenParams,
    mut sess: Session,
    rig: MtpRig,
    load_ms: u128,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let MtpRig { mut tscr, t_seq, tscr_seq } = rig;
    let k = cfg.draft_tokens.clamp(1, 16);
    let t1 = now_ms();
    let mut done = 0usize;
    let mut t_logits = Row::dense(Vec::new());
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let last = end == prompt_ids.len();
        let l = sess.feed(cfg, &prompt_ids[done..end], last)?;
        if last {
            t_logits = l;
        }
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut think = ThinkGuard::new(p.think_budget, p.think_open, tok);
    let loop_guard = LoopGuard::new(p.loop_reps);
    let mut guard_from = 0usize; // see the plain loop
    let mut finish: &'static str = "stop";
    let (mut drafted, mut accepted) = (0usize, 0usize);
    let mut t_fed = prompt_ids.len();

    let recent = out.recent(prompt_ids, p.sample.rep_window);
    let mut pending = pick_row(&mut t_logits, &recent, &p.sample, &mut rng);
    'outer: loop {
        if cfg.eos.contains(&pending) { break; }
        if out.generated.len() >= p.max_new { finish = "length"; break; }
        match out.push(pending) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted, accepted,
                    think_forced: think.forced, images: 0, image_pos: 0,
                });
            }
            Pushed::Gone => break 'outer,
        }
        // -- degenerate loop: same policy as the other loops; lookup makes a
        //    stuck repetition ACCELERATE (its own tail matches perfectly), so
        //    this guard earns its keep here
        if loop_guard.tripped(&out.generated[guard_from..]) && !think.take_loop() {
            finish = "repetition";
            break 'outer;
        }
        // -- budget spent: force the block shut on the real sequence (one
        //    plain pass), same as the model-draft loop
        if think.over(&out) {
            let _ = status(&think.note());
            if !out.push_forced(&think.close) {
                break 'outer;
            }
            guard_from = out.generated.len();
            let mut ids = vec![pending];
            ids.extend_from_slice(&think.close);
            let mut row = sess.feed(cfg, &ids, true)?;
            t_fed += ids.len();
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            pending = pick_row(&mut row, &recent, &p.sample, &mut rng);
            continue 'outer;
        }
        // -- history proposes, longest context first (a 5-gram match predicts
        //    its continuation far better than a bare 3-gram); no match at any
        //    length = one ordinary plain step
        let mut drafts = Vec::new();
        for ng in (LOOKUP_NGRAM..=LOOKUP_NGRAM_MAX).rev() {
            drafts = lookup_propose(prompt_ids, &out.generated, ng, k);
            if !drafts.is_empty() {
                break;
            }
        }
        if drafts.is_empty() {
            let mut row = sess.feed(cfg, &[pending], true)?;
            t_fed += 1;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            pending = pick_row(&mut row, &recent, &p.sample, &mut rng);
            continue 'outer;
        }
        drafted += drafts.len();
        // -- ONE verify pass over [pending, d1..dm] on the target BRANCH
        tscr.copy_from(t_seq, t_fed)?;
        let mut feed: Vec<u32> = Vec::with_capacity(drafts.len() + 1);
        feed.push(pending);
        feed.extend_from_slice(&drafts);
        let mut rows = tscr.feed_all(cfg, &feed)?;
        let tscr_fed = t_fed + feed.len();
        // -- verify: accept while the target's own sample agrees
        let mut acc = 0usize;
        let mut replacement: Option<u32> = None;
        for (i, &d) in drafts.iter().enumerate() {
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let expect = pick_row(&mut rows[i], &recent, &p.sample, &mut rng);
            if expect != d {
                replacement = Some(expect);
                break;
            }
            if cfg.eos.contains(&d) {
                accepted += acc;
                break 'outer;
            }
            if out.generated.len() >= p.max_new {
                finish = "length";
                accepted += acc;
                break 'outer;
            }
            match out.push(d) {
                Pushed::More => {}
                Pushed::Stopped => {
                    let decode_ms = now_ms() - t2;
                    return Ok(GenStats {
                        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                        finish_reason: "stop", text: out.text, drafted, accepted: accepted + acc + 1,
                        think_forced: think.forced, images: 0, image_pos: 0,
                    });
                }
                Pushed::Gone => { accepted += acc + 1; break 'outer; }
            }
            acc += 1;
        }
        accepted += acc;
        if let Some(r) = replacement {
            // partial round: commit only the accepted tokens to the real
            // sequence, abandon the branch (re-branched fresh next round)
            sess.feed(cfg, &feed[..acc + 1], false)?;
            t_fed += acc + 1;
            pending = r;
        } else {
            // full acceptance: adopt the branch, bonus token from the last row
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_row(&mut rows[last], &recent, &p.sample, &mut rng);
        }
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted, accepted,
        think_forced: think.forced, images: 0, image_pos: 0,
    })
}

// -------------------------------------------------------------------- http --

#[derive(Clone, Default)]
struct ChatMsg {
    role: String,
    content: String,
    /// attachments on THIS turn, as the raw file bytes the client uploaded.
    /// They are placed at the head of the turn when the prompt is rendered,
    /// which is how every VLM chat template puts them: picture first, then
    /// the question about it.
    images: Vec<Vec<u8>>,
    /// OpenAI tool history (the /v1 passthrough): calls an assistant turn
    /// carried, as (id, name, arguments). Held here until fold_tool_history
    /// renders them into the trained text form - build_prompt itself never
    /// looks at them.
    tool_calls: Vec<(Option<String>, String, serde_json::Value)>,
    /// role:"tool" only: the call this result answers, and (deprecated in
    /// OpenAI's schema but still widely sent) the function's own name
    tool_call_id: Option<String>,
    tool_name: Option<String>,
}

impl ChatMsg {
    fn text(role: &str, content: impl Into<String>) -> ChatMsg {
        ChatMsg { role: role.into(), content: content.into(), ..ChatMsg::default() }
    }
}

/// OpenAI's message content is either a string or an array of typed parts.
/// The array form is how images arrive, and there are three spellings of it
/// in the wild; all three are accepted, because the alternative is a user
/// whose SDK "supports vision" getting a 400 for reasons they cannot see:
///   {"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}
///   {"type":"input_image","image_url":"data:..."}            (Responses API)
///   {"type":"image","source":{"type":"base64","data":"..."}} (Anthropic)
#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<ImageRef>,
    #[serde(default)]
    source: Option<ImageSource>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageRef {
    Url(String),
    Obj { url: String },
}

#[derive(Deserialize)]
struct ImageSource {
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl<'de> Deserialize<'de> for ChatMsg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ChatMsg, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            role: String,
            #[serde(default)]
            content: Option<RawContent>,
            // OpenAI tool history: an assistant turn that called tools carries
            // them here (content is usually null beside them), and a
            // role:"tool" turn names the call it answers
            #[serde(default)]
            tool_calls: Vec<WireToolCall>,
            #[serde(default)]
            tool_call_id: Option<String>,
            #[serde(default)]
            name: Option<String>,
        }
        #[derive(Deserialize)]
        struct WireToolCall {
            #[serde(default)]
            id: Option<String>,
            function: WireFunction,
        }
        #[derive(Deserialize)]
        struct WireFunction {
            name: String,
            // the spec says a JSON-encoded STRING; a client that sends the
            // object itself is accepted rather than corrected
            #[serde(default)]
            arguments: Option<serde_json::Value>,
        }
        let w = Wire::deserialize(d)?;
        let mut msg = ChatMsg { role: w.role, ..Default::default() };
        msg.tool_call_id = w.tool_call_id;
        msg.tool_name = w.name;
        for c in w.tool_calls {
            let args = match c.function.arguments {
                Some(serde_json::Value::String(s)) => {
                    serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                }
                Some(a) => a,
                None => serde_json::json!({}),
            };
            msg.tool_calls.push((c.id, c.function.name, args));
        }
        match w.content {
            None => {}
            Some(RawContent::Text(t)) => msg.content = t,
            Some(RawContent::Parts(parts)) => {
                let mut text = String::new();
                for p in parts {
                    if let Some(t) = p.text {
                        if !text.is_empty() && !t.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t);
                    }
                    let src = match (&p.image_url, &p.source) {
                        (Some(ImageRef::Url(u)), _) => Some(u.clone()),
                        (Some(ImageRef::Obj { url }), _) => Some(url.clone()),
                        (None, Some(s)) => s.url.clone().or_else(|| s.data.clone()),
                        (None, None) => None,
                    };
                    if let Some(src) = src {
                        msg.images.push(decode_image_src(&src).map_err(serde::de::Error::custom)?);
                    }
                }
                msg.content = text;
            }
        }
        Ok(msg)
    }
}

/// Turn one `image_url` into file bytes. Data URIs (and bare base64, which is
/// what the Anthropic shape sends) only: a remote URL is REFUSED rather than
/// fetched, on both counts that matter here. Fetching would tell a third-party
/// host what this deployment is looking at, which is the whole thing the app
/// exists to avoid, and the fleet's egress is IPv6-only anyway, so most such
/// URLs would fail in a way nobody could diagnose from the outside.
fn decode_image_src(src: &str) -> Result<Vec<u8>, String> {
    let s = src.trim();
    let b64 = if let Some(rest) = s.strip_prefix("data:") {
        let (meta, payload) = rest
            .split_once(',')
            .ok_or("malformed data: URI (no comma before the payload)")?;
        if !meta.contains("base64") {
            return Err("only base64 data: URIs are supported for images".into());
        }
        payload
    } else if s.starts_with("http://") || s.starts_with("https://") {
        return Err(
            "image URLs are not fetched: send the image inline as a base64 data: URI. \
             This app never resolves an attachment against a third-party host - that \
             request would leak what you are looking at, which is the point of running \
             the model in an enclave."
                .into(),
        );
    } else {
        s
    };
    let bytes = b64_decode(b64)?;
    match image_kind(&bytes) {
        // The vision encoder decodes through stb_image, which has no VP8, so a
        // webp is transcoded to JPEG here rather than refused at the door - the
        // playground's canvas already does this for uploads, and this covers
        // everyone else (SDKs, pasted data URIs, the vision-service leg).
        Some("webp") => webp::to_jpeg(&bytes),
        Some(_) => Ok(bytes),
        None => Err(
            "attachment is not a recognisable image (png, jpeg, webp, gif or bmp)".into(),
        ),
    }
}

/// The image format, by magic bytes. Sniffing here rather than trusting the
/// data: URI's own mime type means a mislabelled upload fails in this app with
/// a sentence a user can act on, instead of inside the vision encoder.
fn image_kind(b: &[u8]) -> Option<&'static str> {
    if b.len() < 12 {
        return None;
    }
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("png");
    }
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpeg");
    }
    if b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some("webp");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if b.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

/// Standard base64, tolerating whitespace and missing padding (both are
/// common in hand-assembled requests). No dependency for this: the crate
/// carries a tokenizer and serde and nothing else, and a decoder is 20 lines.
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err("image payload is not valid base64".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.is_empty() {
        return Err("image payload is empty".into());
    }
    Ok(out)
}

/// Request shape shared by /chat (legacy) and /v1/chat/completions (OpenAI).
/// OpenAI fields we don't implement are accepted and ignored.
#[derive(Deserialize)]
struct ChatReq {
    messages: Vec<ChatMsg>,
    #[serde(default)]
    model: Option<String>, // OpenAI field: a model name or volume from /models; absent (or unknown) = the largest
    #[serde(default)]
    target: Option<String>, // Enclave extension: cpu | gpu | auto
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    max_completion_tokens: Option<usize>, // newer OpenAI name
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    top_k: Option<usize>, // extension (common in OSS servers)
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stop: Option<serde_json::Value>, // string or [string]
    #[serde(default)]
    enable_thinking: Option<bool>, // extension: false turns off <think> reasoning (thinking models only)
    #[serde(default)]
    chat_template_kwargs: Option<ChatTemplateKwargs>, // vLLM/SGLang spelling of the same switch
    /// extension (needs config.search): `true` searches every turn, `"auto"`
    /// asks the MODEL whether this turn needs the web, absent/false never
    /// searches. Accepts both shapes so an existing `true` client keeps its
    /// meaning.
    #[serde(default)]
    web_search: Option<serde_json::Value>,
    /// extension (needs config.image): whether the model may decide this turn
    /// wants a PICTURE. Absent takes the deployment's `image.default_on`, which
    /// is true - see ImageConfig. Separate from `web_search` on purpose: the
    /// two send different things to different places, so one switch could only
    /// ever be wrong about one of them.
    #[serde(default)]
    image_gen: Option<serde_json::Value>,
    /// Two meanings, told apart by shape. The boolean is an extension (needs
    /// config.tools): `true` lets the model call the tools this DEPLOYMENT
    /// configured, `false` withholds them, absent takes the deployment's
    /// `default_on`. OpenAI's ARRAY is the PASSTHROUGH (see client_tools):
    /// the client's own functions are rendered into the prompt, the model's
    /// call comes back as `tool_calls`, and the CLIENT executes it.
    #[serde(default)]
    tools: Option<serde_json::Value>,
    /// OpenAI's switch: "none" withholds the tools, anything else is the
    /// default. A client that already speaks OpenAI turns them off the way it
    /// knows how.
    #[serde(default)]
    tool_choice: Option<serde_json::Value>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum WebMode {
    Off,
    /// the model decides, per turn
    Auto,
    /// search unconditionally
    Always,
}

#[derive(Deserialize, Default)]
struct ChatTemplateKwargs {
    #[serde(default)]
    enable_thinking: Option<bool>,
}

impl ChatReq {
    /// The request's thinking switch: top-level `enable_thinking` wins over
    /// `chat_template_kwargs.enable_thinking`; absent means on. Only models
    /// whose config marks them `thinking` act on it (see build_prompt).
    fn thinking(&self) -> bool {
        self.enable_thinking
            .or(self.chat_template_kwargs.as_ref().and_then(|k| k.enable_thinking))
            .unwrap_or(true)
    }

    /// Does this request want the deployment's tools? `tool_choice: "none"`
    /// wins over everything, then the explicit `tools` boolean, then the
    /// deployment's default.
    fn tools_on(&self, default_on: bool) -> bool {
        if self.tool_choice.as_ref().and_then(|v| v.as_str()) == Some("none") {
            return false;
        }
        match self.tools.as_ref().and_then(|v| v.as_bool()) {
            Some(b) => b,
            None => default_on,
        }
    }

    /// The client-declared tool array (OpenAI `tools: [...]`), as a registry
    /// the prompt can render. This is the PASSTHROUGH mode tools.rs describes:
    /// the model is offered the CLIENT's functions, its call comes back on the
    /// reply as `tool_calls`, and the client executes it - nothing here ever
    /// runs one. A request that declares tools gets them INSTEAD of the
    /// deployment's registry, never merged with it: the model sees one list,
    /// and a client-supplied name must not be able to select a server-executed
    /// capability that merely shares it.
    ///
    /// Err when the array is present but not a shape this side can name a
    /// function from - a client waiting for `tool_calls` has to hear why it
    /// will never get one, not receive prose.
    fn client_tools(&self) -> Result<Option<Vec<tools::Tool>>, String> {
        if self.tool_choice.as_ref().and_then(|v| v.as_str()) == Some("none") {
            return Ok(None);
        }
        let Some(arr) = self.tools.as_ref().and_then(|v| v.as_array()) else { return Ok(None) };
        if arr.is_empty() {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(arr.len());
        for (i, t) in arr.iter().enumerate() {
            // OpenAI nests the function under "function"; a flat
            // {"name": ...} entry (the Anthropic/Responses spelling) works too
            let f = t.get("function").unwrap_or(t);
            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("").trim();
            if name.is_empty() {
                return Err(format!(
                    "tools[{i}] names no function: expected \
                     {{\"type\": \"function\", \"function\": {{\"name\": ...}}}}"
                ));
            }
            out.push(tools::Tool {
                name: name.to_string(),
                description: f
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                parameters: tools::object_schema(
                    f.get("parameters").or_else(|| f.get("input_schema")).cloned(),
                ),
                src: tools::ToolSrc::Client,
            });
        }
        Ok(Some(out))
    }

    /// OpenAI's `tool_choice` when it FORCES a call: Some("") for
    /// `"required"` (any tool), Some(name) for
    /// `{"type": "function", "function": {"name": ...}}`. `"auto"`, `"none"`
    /// and absent are all None - only the forcing forms need words in the
    /// prompt.
    fn tool_must_call(&self) -> Option<String> {
        let v = self.tool_choice.as_ref()?;
        if v.as_str() == Some("required") {
            return Some(String::new());
        }
        v.get("function")?.get("name")?.as_str().map(str::to_string)
    }

    /// Does this request let the router reach for an image? `false`/`"off"`
    /// withholds it, `true`/`"auto"` allows it, absent takes the deployment's
    /// default. There is no "always": an image on every turn is not a mode
    /// anyone wants, and `/image ` already forces one.
    fn image_on(&self, default_on: bool) -> bool {
        match &self.image_gen {
            Some(v) if v.as_bool() == Some(true) => true,
            Some(v) if v.as_bool() == Some(false) => false,
            Some(v) if v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("auto")) => true,
            Some(v) if v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("off")) => false,
            _ => default_on,
        }
    }

    fn web_mode(&self) -> WebMode {
        match &self.web_search {
            Some(v) if v.as_bool() == Some(true) => WebMode::Always,
            Some(v) if v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("auto")) => WebMode::Auto,
            _ => WebMode::Off,
        }
    }
}

/// The router pass: ask the model whether answering the last turn needs the
/// web, and if so for what query.
///
/// This is what makes search behave the way people expect from a chat app -
/// "what changed in Rust 1.90" searches, "rewrite that shorter" does not -
/// without the user having to operate a switch. It is a SEPARATE, tiny
/// generation rather than tool-calling in the main pass, for two reasons:
/// this app renders chatml by hand and has no tools template to hang a
/// tool_call off, and a decision made before the answer starts means the
/// results are in the prompt the answer is actually generated from, instead
/// of being stitched in after the model has already committed to an opening.
///
/// Deliberately cheap: thinking OFF, greedy, a few dozen tokens, no drafting.
/// Every failure - a bad decode, a model that ignores the format, a session
/// that will not open - returns None and the turn proceeds WITHOUT search.
/// Routing is an optimisation; it must never be the reason an answer fails.
/// What this turn wants out of the router pass, and what it got back. Both are
/// structs rather than a pile of bools and out-params because the pass answers
/// two independent questions and either half can be switched off.
#[derive(Clone, Copy)]
struct RouterAsk {
    /// SEARCH is one of the verdicts this turn may come back with. False when
    /// the deployment gave the MODEL a web_search tool: the point of the tool
    /// is that the decision happens after it has thought about the question,
    /// so a classifier guessing beforehand would only get in the way (and
    /// would spend a provider round trip the model never asked for).
    search: bool,
    /// ...and IMAGE. Independent of `search`, because a deployment can hand
    /// the model search as a tool while keeping image generation on the router.
    image: bool,
    /// rate how much reasoning the turn needs
    effort: bool,
}

#[derive(Default)]
struct RouterOut {
    verdict: Option<RouterVerdict>,
    effort: Option<Effort>,
}

fn route_web_search(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
    mode: &str,
    ask: RouterAsk,
    on_status: &dyn Fn(&str),
) -> RouterOut {
    let (want_search, want_image, want_effort) = (ask.search, ask.image, ask.effort);
    // the routing half runs when EITHER verdict is on offer
    let want_route = want_search || want_image;
    // The instructions are assembled from the capabilities this deployment
    // ACTUALLY has: offering the model a tool that is not configured is how
    // you get an IMAGE verdict on a deployment with no image service, and a
    // turn that does nothing while the user waits.
    let image_rule = if want_image {
        "\nGenerate an image when the user asks to see, draw, paint, render, \
design or illustrate something, or asks for a picture, photo, logo, icon or \
artwork. Do NOT generate one when they are asking ABOUT an image or about art \
in general, or asking to edit an image you cannot see.\n"
    } else {
        ""
    };
    let image_option = if want_image {
        "IMAGE: <a vivid, self-contained description of the picture to make>\nor\n"
    } else {
        ""
    };
    // The rating and the routing decision share ONE pass. They are asked
    // together because the expensive part is prefilling the conversation tail,
    // not the dozen tokens that come back - two passes would double the cost of
    // the cheap half to keep the prompts tidy.
    // WHY THIS DEFAULTS TO SEARCHING, and why the old wording did not work.
    //
    // The rule used to read "search when the answer depends on information a
    // language model would not reliably know ... or an obscure named entity",
    // which asks the model to predict its own recall. It cannot. Reported live
    // 2026-07-30: "What happened to Omar in the Wire?" routed to NO, and the
    // answer that came back named the wrong season, the wrong killer and a
    // character who had died four seasons earlier - fluently, with no hedge.
    // The Wire is not obscure, so the rule read as "you know this"; the model
    // was certain and wrong, which is the whole failure mode.
    //
    // So the question is no longer "do you know it" but "is it checkable". A
    // fact about a person, work, product or event goes to the provider whether
    // or not the model believes it remembers, and the NO list is what carries
    // the exceptions - the cheap turns that must never spend a round trip.
    //
    // MEASURED against the fable-fusion 27b on a live deployment, 2026-07-30,
    // reproducing this pass exactly (same model, greedy, thinking off): the old
    // wording scored 5/8 on a mixed battery, this one 8/8, and then 7/7 on a
    // second battery written to catch over-searching (thanks / haiku /
    // translate / write a function / opinion / follow-up all still NO). The
    // cost of the change is real and worth stating: more turns now spend a
    // provider round trip, and with fetch_pages set, page fetches too.
    let search_rules = if want_search {
        "DEFAULT TO SEARCHING. If answering involves any specific fact about the world - a \
person, place, work, product, organisation or event, real or fictional; a date, number, name, \
outcome, plot point, credit or biography - then SEARCH, even when you are certain you remember \
it. Model recall of these details is wrong often enough, and confidently enough, that the reader \
cannot tell. Mechanical test: if the answer could be looked up on a reference site, SEARCH.

Search too whenever the answer depends on information that changes: current events, news, \
prices, weather, sports results, release versions, live status, or anything dated after your \
training.

Answer NO only for: chit-chat, greetings and thanks, translation, summarising \
or rewriting text already in the conversation, arithmetic, code the model can \
simply write, definitions of well-established concepts, opinions, creative \
writing, and follow-ups answerable from what was already said.
"
    } else {
        ""
    };
    let route_rules = format!("{search_rules}{image_rule}");
    let effort_rules = if want_effort {
        "Rate how much step-by-step REASONING the last message needs before it can be \
answered well:
low = greetings, thanks, chit-chat, rewriting or translating text already here, \
a fact you simply know, a one-line answer.
medium = ordinary explanation, short code, familiar multi-step work.
high = proofs and derivations, tricky debugging, careful comparison of several \
options, anything where a wrong intermediate step ruins the answer.
"
    } else {
        ""
    };
    // only the verdicts actually on offer are named: a model shown a SEARCH
    // line it must not use will eventually use it
    let search_option = if want_search { "SEARCH: <the query to run>\nor\n" } else { "" };
    let route_line = if want_route {
        format!("{search_option}{image_option}NO")
    } else {
        String::new()
    };
    let reply_form = match (want_route, want_effort) {
        // both: one line, rating first, so a truncated reply still carries it
        (true, true) => format!(
            "Reply with EXACTLY ONE line and nothing else, in this form:\n\
             EFFORT: <low|medium|high> | <one of the following>\n{route_line}"
        ),
        (true, false) => format!("Reply with EXACTLY ONE line and nothing else:\n{route_line}"),
        (false, true) => "Reply with EXACTLY ONE line and nothing else:\n\
                          EFFORT: <low|medium|high>"
            .to_string(),
        (false, false) => return RouterOut::default(),
    };
    let router_system = format!(
        "You decide what is needed to handle the user's last message.

{route_rules}{effort_rules}
{reply_form}"
    );
    let router_system = router_system.as_str();

    // Only the tail of the conversation matters for this decision, and a long
    // history would dominate the router's own budget.
    let mut router_msgs: Vec<ChatMsg> = vec![ChatMsg::text("system", router_system)];
    let tail: Vec<&ChatMsg> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .rev()
        .take(4)
        .collect();
    for m in tail.into_iter().rev() {
        // TEXT ONLY, deliberately: the router is a cheap classifier and
        // re-encoding every attached picture for it would cost more than the
        // answer it is routing. An image turn with no words still says
        // something useful here, so it is announced rather than dropped.
        let mut c = truncate_for_msg_n(&m.content, 1500);
        if !m.images.is_empty() {
            let note = if m.images.len() == 1 {
                "[the user attached an image]".to_string()
            } else {
                format!("[the user attached {} images]", m.images.len())
            };
            c = if c.trim().is_empty() { note } else { format!("{note}\n{c}") };
        }
        router_msgs.push(ChatMsg::text(&m.role, c));
    }

    // thinking off: this is a classifier, not a reasoning task
    let Ok((ids, stops, _)) = build_prompt(cfg, tok, &router_msgs, false, Capabilities::Internal) else {
        return RouterOut::default();
    };
    let params = GenParams {
        max_new: 48,
        sample: SampleParams {
            temperature: 0.0, // greedy: the same question routes the same way
            top_p: 1.0,
            top_k: 0,
            rep_penalty: 1.0,
            rep_window: 0,
        },
        stop_strings: {
            let mut s = stops;
            s.push("\n".into()); // one line, then stop
            s
        },
        think_budget: 0,
        think_open: false,
        // the router emits one short line; a loop there is still a loop
        loop_reps: 4,
    };
    let Some(&(target, tname)) = targets_for(cfg, mode).first() else {
        return RouterOut::default();
    };
    let noop_emit = |_: &str| true;
    let status = internal_status(
        if want_route { "deciding what this needs…" } else { "sizing the reasoning budget…" },
        on_status,
        INTERNAL_BUSY_BUDGET_MS,
    );
    let Ok(stats) = generate(
        cfg, tok, &ids, target, tname, &params, &DraftPlan::Plain, &noop_emit, &status,
    ) else {
        return RouterOut::default();
    };
    RouterOut {
        // a rating the model did not give leaves the flat budget in place
        effort: want_effort.then(|| parse_effort(&stats.text)).flatten(),
        verdict: want_route
            .then(|| parse_router_verdict(&stats.text, want_search, want_image))
            .flatten(),
    }
}

/// A reply that is really a fabricated tool call, and the query inside it.
///
/// Models under a "call a search tool" instruction emit these in half a dozen
/// dialects: `<tool_code>`, ```tool_code fences, `<tool_call>` with JSON, a bare
/// `search_tool(query="…")`. This app has no tool API, so nothing executes any
/// of them and the user is left looking at the call. Rather than argue with the
/// prompt that caused it, take the query the model asked for and run the search
/// it wanted (see the /chat path).
///
/// STRICT on purpose. It fires only when the call is essentially the WHOLE
/// reply, because a legitimate answer may quote tool syntax - someone asking
/// "how do I write a tool_code block?" must get their answer, not a web search.
fn fabricated_tool_query(text: &str) -> Option<String> {
    let body = strip_think(text);
    let t = body.trim();
    if t.is_empty() || t.len() > 600 {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // the call has to OPEN the reply, not appear somewhere inside a real answer
    let opens = ["<tool_code", "<tool_call", "```tool", "```json", "<function", "{\"name\":"];
    let starts_with_call = opens.iter().any(|p| lower.starts_with(p));
    let bare_call = lower.starts_with("search") && t.contains('(') && t.contains(')');
    if !starts_with_call && !bare_call {
        return None;
    }
    // and it has to look like a SEARCH, not some other invented tool
    if !lower.contains("search") && !lower.contains("browse") && !lower.contains("web") {
        return None;
    }
    // the query: the first quoted string long enough to be one
    let mut q = None;
    for quote in ['"', '\''] {
        let mut parts = t.split(quote);
        parts.next(); // before the first quote
        while let Some(cand) = parts.next() {
            let c = cand.trim();
            // skip the argument NAMES that sit in quotes in JSON dialects
            if c.len() >= 3 && !matches!(c.to_ascii_lowercase().as_str(),
                "query" | "q" | "search" | "name" | "arguments" | "parameters" | "web_search"
                | "search_tool" | "type" | "function")
            {
                q = Some(c.to_string());
                break;
            }
            parts.next(); // the text between this closing quote and the next opening one
        }
        if q.is_some() {
            break;
        }
    }
    let q = q?;
    if q.chars().count() < 3 || q.chars().count() > 300 {
        return None;
    }
    Some(q)
}

/// Arm the ONE retry after a fabricated tool call with something the model
/// cannot read past: it just wrote a call, nothing ran it, and the answer has
/// to come from what it knows. Written into the SYSTEM turn rather than as a
/// user message, because it is a fact about the deployment and not something
/// the person at the keyboard said.
fn no_tools_nudge(cfg: &AppConfig, messages: &mut Vec<ChatMsg>, have_results: bool) {
    const NO_WEB: &str = "IMPORTANT: you just wrote a tool call. Nothing executed it, because this \
deployment gives you no tools at all. Do not write another one. Answer the question now, in prose, \
from your own knowledge, and say plainly at the end that the answer is unverified.";
    const HAVE_WEB: &str = "IMPORTANT: you just wrote a tool call. Nothing executed it, because \
this deployment gives you no tools at all - the search already ran and its results are in the \
user's message. Do not write another call. Answer the question now, in prose, from those results, \
citing them as [1], [2].";
    let nudge = if have_results { HAVE_WEB } else { NO_WEB };
    match messages.iter_mut().find(|m| m.role == "system") {
        Some(sys) => sys.content = format!("{}\n\n{nudge}", sys.content.trim_end()),
        // no system turn in the request means build_prompt falls back to the
        // deployment's own prompt, so carry that across rather than dropping it
        None => messages.insert(
            0,
            ChatMsg::text("system", format!("{}\n\n{nudge}", cfg.system_prompt.trim_end())),
        ),
    }
}

// ------------------------------------------------------------- tool calls --

/// The tool-calling loop, shared by the playground and both /v1 shapes.
///
/// Each transport narrates differently - SSE events on /chat, SSE comments on
/// streaming /v1, nothing at all on buffered /v1 - so the loop keeps the STATE
/// and the decisions, and the caller keeps the writing. One `step` per finished
/// generation: it returns true when it has appended a call and its result to
/// the conversation and the answer should be generated again.
struct ToolLoop<'a> {
    cfg: &'a tools::ToolsConfig,
    /// what the built-in tools are wired to (the deployment's search leg)
    builtins: tools::Builtins<'a>,
    reg: tools::Registry,
    calls: usize,
    /// the model was already told it has run out of calls. Without this a model
    /// that keeps calling would loop forever: told, calls again, told again.
    limit_told: bool,
    /// what ran, for the reply's stats
    log: Vec<serde_json::Value>,
}

impl<'a> ToolLoop<'a> {
    /// Resolve the registry for this turn. MCP discovery happens HERE, before
    /// the prompt exists, because the schemas have to be in it.
    fn open(
        cfg: &'a tools::ToolsConfig,
        builtins: tools::Builtins<'a>,
        on_status: &dyn Fn(&str),
    ) -> ToolLoop<'a> {
        ToolLoop {
            cfg,
            builtins,
            reg: tools::build(cfg, builtins, on_status),
            calls: 0,
            limit_told: false,
            log: Vec::new(),
        }
    }

    /// The model owns the search decision this turn, so the router must not
    /// also make it.
    fn owns_search(&self) -> bool {
        self.reg.find("web_search").is_some()
    }

    fn armed(&self) -> bool {
        !self.reg.is_empty()
    }

    fn tools(&self) -> &[tools::Tool] {
        &self.reg.tools
    }

    /// Handle one finished generation. `on_call` fires before the round trip
    /// (so a slow tool is visible while it runs) and `on_result` after.
    /// Returns true when the answer should be regenerated.
    fn step(
        &mut self,
        text: &str,
        messages: &mut Vec<ChatMsg>,
        on_call: &dyn Fn(&serde_json::Value),
        on_result: &dyn Fn(&serde_json::Value),
    ) -> bool {
        if !self.armed() {
            return false;
        }
        let Some(c) = tools::parse_calls(text).into_iter().next() else { return false };
        // Out of calls: say so once and let it write the answer. Saying it
        // twice is a loop, so the second offence is delivered to the user as
        // it stands - a visible fake call beats an endless turn.
        if self.calls >= self.cfg.max_calls {
            if self.limit_told {
                return false;
            }
            self.limit_told = true;
            messages.push(ChatMsg::text("assistant", canonical_call(&c)));
            messages.push(ChatMsg::text(
                "user",
                tools::response_turn(
                    &c.name,
                    "This call was NOT run: the limit on tool calls for one answer has been \
                     reached. Do not call anything else. Answer now from what you already have, \
                     and say plainly what you could not check.",
                ),
            ));
            return true;
        }
        self.calls += 1;
        on_call(&serde_json::json!({
            "name": c.name, "arguments": c.args, "n": self.calls,
        }));
        let r = tools::call(&mut self.reg, self.cfg, self.builtins, &c.name, &c.args, || {
            now_ms() as u64
        });
        let mut entry = serde_json::json!({
            "name": c.name, "arguments": c.args, "n": self.calls,
            "ok": !r.is_error, "ms": r.ms,
            "chars": r.text.chars().count(),
        });
        // a search the MODEL asked for gets the same numbered source list a
        // routed one does, so its [1] and [2] resolve to something
        if !r.sources.is_empty() {
            // the SAME shape search_meta_json emits, so the playground's
            // existing source list renders it without knowing where it came from
            entry["sources"] = serde_json::json!(r
                .sources
                .iter()
                .map(|(t, u)| serde_json::json!({ "title": t, "url": u }))
                .collect::<Vec<_>>());
        }
        on_result(&entry);
        self.log.push(entry);
        // The model's own call goes back in as the assistant turn it was, so
        // the next pass sees what it asked for beside what came back.
        messages.push(ChatMsg::text("assistant", canonical_call(&c)));
        messages.push(ChatMsg::text("user", tools::response_turn(&c.name, &r.text)));
        true
    }
}

/// Holds back the beginning of an answer just long enough to tell whether it
/// is a tool call this app is about to execute itself.
///
/// Without it every call flashes on screen as raw JSON before the client is
/// told to drop it, and on /v1 - whose protocol has no "ignore that" event at
/// all - the JSON simply stays in the content. Reasoning is NOT held: the
/// decision point is the first thing written after the think block closes, so
/// a model that reasons for thirty seconds still streams for thirty seconds.
///
/// The gate holds only while what it has could still BECOME an opener, so the
/// worst case is a dozen characters of latency on an ordinary answer.
struct CallGate {
    /// the answer has been judged: everything from here on flows (or is
    /// dropped, if it was a call)
    decided: bool,
    suppress: bool,
    /// the prompt force-opened a think block, so the body starts after the
    /// closing tag rather than at the first token
    thinking: bool,
    held: String,
}

/// What an answer looks like when it is really a call. A model that opens with
/// either of these is asking for a tool, not writing prose.
const CALL_OPENERS: [&str; 2] = ["<tool_call>", "{\"name\""];

impl CallGate {
    fn new(armed: bool, think_open: bool) -> CallGate {
        CallGate { decided: !armed, suppress: false, thinking: think_open, held: String::new() }
    }

    /// Feed one delta; returns what should go out to the client now.
    fn push(&mut self, delta: &str) -> Option<String> {
        if self.decided {
            // a suppressed call keeps accumulating, because it still has to be
            // deliverable if it turns out nothing executed it
            if self.suppress {
                self.held.push_str(delta);
                return None;
            }
            return Some(delta.to_string());
        }
        self.held.push_str(delta);
        if self.thinking {
            // inside the reasoning block nothing is held back
            let Some(i) = self.held.find("</think>") else {
                return Some(std::mem::take(&mut self.held));
            };
            let head = self.held[..i + "</think>".len()].to_string();
            self.held = self.held[i + "</think>".len()..].to_string();
            self.thinking = false;
            return match self.judge() {
                Some(rest) => Some(format!("{head}{rest}")),
                None => (!head.is_empty()).then_some(head),
            };
        }
        self.judge()
    }

    /// Look at the body written so far and decide, or keep waiting.
    fn judge(&mut self) -> Option<String> {
        let trimmed = self.held.trim_start();
        if trimmed.is_empty() {
            return None;
        }
        if CALL_OPENERS.iter().any(|o| trimmed.starts_with(o)) {
            self.decided = true;
            self.suppress = true;
            return None;
        }
        // still ambiguous: what we have could yet grow into an opener
        if CALL_OPENERS.iter().any(|o| o.starts_with(trimmed)) {
            return None;
        }
        self.decided = true;
        Some(std::mem::take(&mut self.held))
    }

    /// Generation ended with text still held - either an ordinary answer too
    /// short to have been judged, or a call nothing executed. Either way it
    /// belongs to the user now: silence would be the worse failure.
    fn flush(&mut self) -> Option<String> {
        self.decided = true;
        self.suppress = false;
        let out = std::mem::take(&mut self.held);
        (!out.is_empty()).then_some(out)
    }

    /// The held text was a call the CLIENT will execute (the /v1 passthrough):
    /// drop it, because it leaves as structured `tool_calls` rather than as
    /// content. A gate that was not suppressing holds prose, which the flush
    /// after this still delivers.
    fn drop_call(&mut self) {
        if self.suppress {
            self.suppress = false;
            self.held.clear();
        }
    }
}

/// The call, rewritten in the trained form. The raw reply may carry a
/// half-open tag (the stop string ate the closer) or a fence, and feeding that
/// back would teach the model its own malformed output was acceptable.
fn canonical_call(c: &tools::ToolCall) -> String {
    format!(
        "<tool_call>\n{}\n</tool_call>",
        serde_json::json!({ "name": c.name, "arguments": c.args })
    )
}

// ------------------------------------------- client tools (the passthrough) --

/// Rewrite OpenAI tool history into the trained chatml text forms: an
/// assistant `tool_calls` array becomes `<tool_call>` blocks in that turn's
/// content, a `role:"tool"` result becomes a `<tool_response>` block in a
/// USER turn (Qwen's own template renders it exactly there), and consecutive
/// results share one turn the way that template merges them. The result names
/// itself by matching its `tool_call_id` against the calls already seen,
/// because OpenAI's schema stopped sending the function name alongside.
///
/// Runs on every /v1 request - a turn with none of these shapes passes
/// through untouched - and never on /chat, whose client is the playground.
fn fold_tool_history(messages: &mut Vec<ChatMsg>) {
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out: Vec<ChatMsg> = Vec::with_capacity(messages.len());
    for mut m in messages.drain(..) {
        if m.role == "assistant" && !m.tool_calls.is_empty() {
            let mut content = m.content.trim_end().to_string();
            for (id, name, args) in std::mem::take(&mut m.tool_calls) {
                if let Some(id) = id {
                    names.insert(id, name.clone());
                }
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&canonical_call(&tools::ToolCall { name, args }));
            }
            m.content = content;
            out.push(m);
        } else if m.role == "tool" {
            let name = m
                .tool_name
                .clone()
                .or_else(|| m.tool_call_id.as_ref().and_then(|id| names.get(id).cloned()))
                .unwrap_or_default();
            let block = tools::response_turn(&name, &m.content);
            match out.last_mut() {
                Some(prev) if prev.role == "user" && prev.content.starts_with("<tool_response>") =>
                {
                    prev.content.push('\n');
                    prev.content.push_str(&block);
                }
                _ => out.push(ChatMsg::text("user", block)),
            }
        } else {
            out.push(m);
        }
    }
    *messages = out;
}

/// The parsed calls in OpenAI's reply shape. `arguments` is a JSON-encoded
/// STRING - that is the spec, and every SDK parses it back. Ids are
/// synthesized from `seed` (the clock): this side never saw an id, the client
/// only needs them to pair results with calls, and a deterministic input
/// keeps this testable.
fn openai_tool_calls(calls: &[tools::ToolCall], seed: u128, streaming: bool) -> serde_json::Value {
    serde_json::Value::Array(
        calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut v = serde_json::json!({
                    "id": format!("call_{seed:x}_{i}"),
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.args.to_string() },
                });
                // the chunk schema addresses calls by index; the buffered
                // message schema has no such field, so it is not invented there
                if streaming {
                    v["index"] = serde_json::json!(i);
                }
                v
            })
            .collect(),
    )
}

/// What of a reply that ended in a call is still CONTENT: the reasoning and
/// any prose before it. With the trained form that is everything before the
/// first `<tool_call>` of the BODY (a call discussed inside <think> is
/// reasoning, not a call - same rule as parse_calls); with the bare-object
/// form the whole body was the call, so only the think block survives.
fn visible_before_call(text: &str) -> &str {
    let body = text.rfind("</think>").map_or(0, |i| i + "</think>".len());
    let cut = match text[body..].find("<tool_call>") {
        Some(i) => body + i,
        None => body,
    };
    text[..cut].trim_end()
}

/// Whether this request gets the deployment's tools: the config has some, the
/// model's template is one tool calling was trained into, and the client did
/// not opt out (nor is it a client that has not opted IN, when the deployment
/// leaves `default_on` false).
/// Everything the built-ins could be wired to. Used by the /tools probe, which
/// answers "what does this deployment have", not "what may this turn use".
fn builtins_of(cfg: &AppConfig) -> tools::Builtins<'_> {
    tools::Builtins { search: cfg.search.as_ref(), web_withheld: false }
}

/// ...and what THIS turn may use. The web_search tool is still web search, so
/// the user's search switch governs it exactly as it governs the router: off
/// means the tool is not in the registry and the model is never told it exists.
/// Without this, turning the router off would hand the same capability straight
/// back through the tools switch, which is the opposite of what either control
/// says on the tin.
fn builtins_for<'a>(cfg: &'a AppConfig, creq: &ChatReq) -> tools::Builtins<'a> {
    let withheld = creq.web_mode() == WebMode::Off;
    tools::Builtins {
        search: if withheld { None } else { cfg.search.as_ref() },
        web_withheld: withheld,
    }
}

fn tools_enabled<'a>(cfg: &'a AppConfig, creq: &ChatReq) -> Option<&'a tools::ToolsConfig> {
    let tc = cfg.tools.as_ref()?;
    if tc.is_empty() || !tools::template_supported(&cfg.template) {
        return None;
    }
    creq.tools_on(tc.default_on).then_some(tc)
}

/// Name a conversation from its opening exchange, for the history list.
///
/// The playground has always had a title: the first message, cut at 64
/// characters. That is free and it is bad - "can you help me figure out why my
/// dock" is not a name, it is a fragment. This asks the model for one instead.
///
/// EFFICIENCY, which is the whole design: the pass runs AFTER the answer has
/// streamed, from a route of its own, so it is never in front of anything the
/// user is waiting for. The reply is on screen; the sidebar entry renames
/// itself a moment later. It costs one short greedy generation over a short
/// prompt - the question plus the first few lines of the answer, not the
/// conversation - and it happens ONCE per chat, on the first exchange only.
///
/// The answer is included because it is what rescues a vague opener: "help me
/// with this error" names nothing, and the reply that follows names it exactly.
fn make_title(
    cfg: &AppConfig,
    tok: &Tokenizer,
    question: &str,
    answer: &str,
    mode: &str,
) -> Option<String> {
    let system = "You name conversations for a chat history list, like a file name.

Rules:
- 3 to 6 words, under 48 characters.
- Name the SUBJECT, not the interaction: \"Rust mutex deadlock\", never \"User asks for help\".
- No quotes, no trailing period, no markdown, no emoji.
- Sentence case, keeping the capitalisation of names and code (Rust, useEffect, K8s).
- If the opening message is only a greeting, name it Greeting.

Reply with EXACTLY ONE line: the title and nothing else.";

    let mut msgs: Vec<ChatMsg> = vec![ChatMsg::text("system", system)];
    let q = truncate_for_msg_n(question, 600);
    let a = truncate_for_msg_n(strip_think(answer).trim(), 300);
    let opening = if a.is_empty() {
        format!("Conversation opens with:\n{q}")
    } else {
        format!("Conversation opens with:\n{q}\n\nThe reply began:\n{a}")
    };
    msgs.push(ChatMsg::text("user", opening));

    // thinking off: naming a thing is not a reasoning task
    let (prompt, stops, _) = build_prompt(cfg, tok, &msgs, false, Capabilities::Internal).ok()?;
    let params = GenParams {
        max_new: 24,
        sample: SampleParams {
            temperature: 0.0, // greedy: the same chat gets the same name
            top_p: 1.0,
            top_k: 0,
            rep_penalty: 1.0,
            rep_window: 0,
        },
        stop_strings: {
            let mut s = stops;
            s.push("\n".into());
            s
        },
        think_budget: 0,
        think_open: false,
        loop_reps: 4,
    };
    let (target, tname) = *targets_for(cfg, mode).first()?;
    let noop_emit = |_: &str| true;
    // no stream to narrate to (the client fires-and-forgets titles), but the
    // budget still applies: a name is never worth five minutes in the queue
    let status = internal_status("naming the chat", &|_: &str| {}, INTERNAL_BUSY_BUDGET_MS);
    let stats = generate(
        cfg, tok, &prompt, target, tname, &params, &DraftPlan::Plain, &noop_emit, &status,
    )
    .ok()?;
    clean_title(&stats.text)
}

/// Make a model's line usable as a title, or reject it. Models wrap titles in
/// quotes, prefix them with "Title:", bold them, and add a full stop; a title
/// list is one place where that noise is glaring, and none of it is worth a
/// retry when trimming is deterministic.
fn clean_title(raw: &str) -> Option<String> {
    let line = strip_think(raw)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .to_string();
    let mut t = line.trim().to_string();
    // peel the wrappers in a loop: `**"Title: x"**` needs several passes, and
    // one pass of each leaves the others' leftovers behind
    loop {
        let before = t.clone();
        t = t.trim().trim_matches(['"', '\'', '`', '*', '#', '_']).trim().to_string();
        for p in ["Title:", "title:", "TITLE:", "Chat title:"] {
            if let Some(rest) = t.strip_prefix(p) {
                t = rest.trim().to_string();
            }
        }
        t = t.trim_end_matches(['.', ',', ';', ':', '!']).trim().to_string();
        if t == before {
            break;
        }
    }
    // collapse any internal whitespace, including a stray newline the stop
    // string did not catch
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() || t.chars().count() > 80 {
        return None; // a paragraph is not a title; keep the client's fallback
    }
    // hard cap on a word boundary, so a long-winded title truncates readably
    let t = if t.chars().count() > 48 {
        let mut cut = t
            .char_indices()
            .take_while(|(i, _)| *i <= 48)
            .map(|(i, _)| i)
            .last()
            .unwrap_or(48);
        if let Some(sp) = t[..cut].rfind(' ') {
            if sp > 20 {
                cut = sp;
            }
        }
        t[..cut].trim_end().to_string()
    } else {
        t
    };
    Some(t)
}

/// What the router decided.
#[derive(PartialEq, Debug)]
enum RouterVerdict {
    Search(String),
    Image(String),
}

/// How much reasoning the turn was rated to need. Three classes, not a 0-100
/// score: a model asked for a number produces a confident-looking one it cannot
/// actually calibrate, while "is this chit-chat, ordinary work, or hard?" is a
/// judgement it makes well and a human can check.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    fn parse(s: &str) -> Option<Effort> {
        let s = s.trim().trim_matches(['"', '\'', '`', '*', '.']).trim();
        match s.to_ascii_lowercase().as_str() {
            "low" | "simple" | "easy" => Some(Effort::Low),
            "medium" | "moderate" | "normal" => Some(Effort::Medium),
            "high" | "hard" | "complex" => Some(Effort::High),
            _ => None,
        }
    }

    /// The reasoning ceiling for a turn of this class. Clamped BOTH ways: never
    /// above the model's own think_budget (effort scaling only ever spends
    /// less), never below the floor (a misrating must not cost real reasoning),
    /// and 0 anywhere still means uncapped, so a deployment that left
    /// think_budget at 0 does not silently gain a cap it never asked for.
    fn budget(self, cfg: &AppConfig) -> usize {
        let Some(e) = &cfg.effort else {
            return cfg.think_budget;
        };
        let want = match self {
            Effort::Low => e.low,
            Effort::Medium => e.medium,
            Effort::High => e.high,
        };
        if want == 0 {
            return cfg.think_budget; // "no reduction for this class"
        }
        let want = want.max(e.floor);
        if cfg.think_budget == 0 {
            return want;
        }
        want.min(cfg.think_budget)
    }

    fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// Drop a leading `EFFORT: <class> |` from a router line, leaving the verdict
/// half untouched - including any `|` inside a query.
fn strip_effort_prefix(line: &str) -> &str {
    let l = line.trim_start().trim_start_matches(['-', '*', '#', '>', ' ']);
    let l = l.trim_start_matches("**").trim_start();
    if !l.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("effort:")) {
        return line;
    }
    match l.split_once('|') {
        Some((_, rest)) => rest,
        None => line,
    }
}

/// Pull `EFFORT: <class>` out of the router's reply. Separate from the verdict
/// parse because the two travel on the same line and either can be missing: a
/// model that answers the search question and ignores the rating is still worth
/// listening to about search.
fn parse_effort(text: &str) -> Option<Effort> {
    let cleaned = strip_think(text);
    for line in cleaned.lines() {
        for seg in line.split('|') {
            let s = seg.trim().trim_start_matches(['-', '*', '#', '>', ' ']).trim();
            let s = s.trim_start_matches("**").trim();
            // `continue`, never `?`: the verdict rides the same line, so most
            // segments are not the rating and finding one is the exception
            let Some(head) = s.get(..7) else { continue };
            if !head.eq_ignore_ascii_case("effort:") {
                continue;
            }
            if let Some(e) = Effort::parse(&s[7..]) {
                return Some(e);
            }
        }
    }
    None
}

/// Pull the verdict out of the router's reply. Tolerant of the usual model
/// noise (a stray think block, markdown bold, quotes, a leading bullet)
/// because the cost of a false NO is silently skipping work the user wanted,
/// and the cost of a false positive is one wasted call.
///
/// `allow_search` / `allow_image` gate the verdicts rather than trusting the
/// prompt: a model that hallucinates a capability the deployment never
/// configured should be ignored, not obeyed. Search is gated for a second
/// reason too - when the MODEL holds a web_search tool, a router that
/// pre-fetched anyway would spend a round trip on a decision it no longer owns.
fn parse_router_verdict(text: &str, allow_search: bool, allow_image: bool) -> Option<RouterVerdict> {
    let cleaned = strip_think(text);
    // With effort scaling on the rating shares the line (`EFFORT: high |
    // SEARCH: …`), so a line-anchored match would find the rating, fail, and
    // silently skip a search the user wanted. Only the RATING prefix is split
    // off: splitting the whole line on `|` would truncate a query that
    // contains one.
    for line in cleaned.lines().map(strip_effort_prefix) {
        let l = line.trim().trim_start_matches(['-', '*', '#', '>', ' ']).trim();
        let l = l.trim_start_matches("**").trim();
        let (rest, is_image) = match find_verdict_prefix(l) {
            Some(v) => v,
            None => continue,
        };
        if (is_image && !allow_image) || (!is_image && !allow_search) {
            continue;
        }
        // markers and whitespace interleave (`**SEARCH:** "q"`), so one pass
        // of each leaves the other's leftovers behind - alternate to a fixed
        // point instead
        let mut q = rest;
        loop {
            let next = q.trim().trim_matches(['"', '\'', '`', '*']);
            if next == q {
                break;
            }
            q = next;
        }
        if !q.is_empty() {
            // an image prompt carries detail and deserves more room than a
            // search query, which engines truncate anyway
            let q = truncate_for_msg_n(q, if is_image { 1000 } else { 300 });
            return Some(if is_image {
                RouterVerdict::Image(q)
            } else {
                RouterVerdict::Search(q)
            });
        }
    }
    None
}

/// `(payload, is_image)` for a verdict line, case-insensitively.
fn find_verdict_prefix(line: &str) -> Option<(&str, bool)> {
    for (tag, is_image) in [("SEARCH:", false), ("IMAGE:", true)] {
        if line.len() >= tag.len() && line[..tag.len()].eq_ignore_ascii_case(tag) {
            return Some((&line[tag.len()..], is_image));
        }
    }
    None
}

fn truncate_for_msg_n(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// What a completed search leg reports back to the client.
// ------------------------------------------------------ vision (images IN) --

/// Who reads the picture on this turn.
#[derive(PartialEq, Debug)]
enum VisionPlan {
    /// the SERVING model reads it itself: its volume carries a projector, and
    /// the image goes into its own prompt (the path in build_prompt/generate)
    Local,
    /// a sibling vision deployment reads it and this app folds the answer in
    Delegate,
}

/// Decide it once, here, so both request paths agree.
///
/// An explicitly NAMED vision model always keeps the picture local: a request
/// that said `"model": "qwen3-vl-8b"` asked for that model to look, and a
/// deployment default has no business overriding it. Everything else prefers
/// the service when one is configured, because that is what configuring one
/// means - the sibling exists so the chat model does not have to be a VLM.
fn vision_plan(cfg: &AppConfig, requested: Option<&str>) -> VisionPlan {
    let Some(vcfg) = &cfg.vision_service else { return VisionPlan::Local };
    let can_local = cfg.vision && cfg.backend == "ggml";
    let named_this = requested
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .is_some_and(|r| r == cfg.name || r == cfg.model_volume);
    if can_local && (named_this || vcfg.prefer_local) {
        return VisionPlan::Local;
    }
    VisionPlan::Delegate
}

/// What the delegated look cost and what was asked, for the reply's metadata.
/// Only ever produced for a DELEGATED turn: when the serving model reads the
/// picture itself there is no second party to report on, and the per-image
/// position count already comes back in the generation stats.
struct VisionMeta {
    model: Option<String>,
    question: String,
    images: usize,
    image_tokens: usize,
    ms: u64,
}

fn vision_meta_json(m: &VisionMeta) -> serde_json::Value {
    serde_json::json!({
        "model": m.model,
        // the question this app's own model wrote on the user's behalf. Shown,
        // not hidden: a user is entitled to know what was asked about their
        // picture, and it is also the first thing to look at when an answer
        // comes back about the wrong detail.
        "question": m.question,
        "images": m.images,
        "image_tokens": m.image_tokens,
        "ms": m.ms,
    })
}

/// Have THIS deployment's model write the question for the vision model.
///
/// This is the step that makes delegation more than a caption sidecar. The
/// vision model sees only the image and this one line - it has no access to the
/// conversation - so the line has to CARRY whatever from the conversation the
/// answer depends on: the spec the user pasted, the value they expect, the
/// thing they said was wrong last time. A model that has read the conversation
/// can write that; a fixed "describe this image" cannot, because it has to
/// choose what matters before anyone has said.
///
/// Returns None on any failure, and the caller falls back to the user's own
/// words - one cheap generation is worth a better question, but never worth
/// losing the turn.
fn author_vision_query(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
    mode: &str,
    on_status: &dyn Fn(&str),
) -> Option<String> {
    let system = "A vision model is about to look at the image the user just attached. It sees \
ONLY the image and the one question you write - it cannot see this conversation, and it will \
not be asked again on this turn.

Write the question. Rules:
- Self-contained: name any detail from the conversation the answer depends on (a value to check \
against, a spec to compare with, what the user said was wrong).
- Ask for what the user actually needs, AND for a short description of the rest of the image, so \
an obvious follow-up does not need a second look.
- Ask for exact transcription when text, numbers or labels matter.
- Never answer it yourself, never refer to \"the user\" or \"the conversation\".

Reply with EXACTLY ONE line and nothing else:
ASK: <the question>";

    let mut msgs: Vec<ChatMsg> = vec![ChatMsg::text("system", system)];
    // Only the tail matters, and a long history would dominate this small
    // generation's own budget.
    let tail: Vec<&ChatMsg> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .rev()
        .take(4)
        .collect();
    for m in tail.into_iter().rev() {
        // TEXT ONLY: re-encoding the picture for the model that is delegating
        // BECAUSE it cannot see would be absurd. The attachment is announced.
        let mut c = truncate_for_msg_n(&m.content, 1500);
        if !m.images.is_empty() {
            let note = if m.images.len() == 1 {
                "[the user attached an image here]".to_string()
            } else {
                format!("[the user attached {} images here]", m.images.len())
            };
            c = if c.trim().is_empty() { note } else { format!("{note}\n{c}") };
        }
        msgs.push(ChatMsg::text(&m.role, c));
    }

    // thinking off: this is a one-line writing task, not a reasoning one
    let (prompt, stops, _) = build_prompt(cfg, tok, &msgs, false, Capabilities::Internal).ok()?;
    let params = GenParams {
        max_new: 120,
        sample: SampleParams {
            temperature: 0.0, // greedy: the same turn asks the same question
            top_p: 1.0,
            top_k: 0,
            rep_penalty: 1.0,
            rep_window: 0,
        },
        stop_strings: {
            let mut s = stops;
            s.push("\n".into()); // one line, then stop
            s
        },
        think_budget: 0,
        think_open: false,
        loop_reps: 4,
    };
    let (target, tname) = *targets_for(cfg, mode).first()?;
    let noop_emit = |_: &str| true;
    let status = internal_status(
        "working out what to ask about the image…",
        on_status,
        INTERNAL_BUSY_BUDGET_MS,
    );
    let stats = generate(
        cfg, tok, &prompt, target, tname, &params, &DraftPlan::Plain, &noop_emit, &status,
    )
    .ok()?;
    parse_ask_line(&stats.text)
}

/// Pull the question out of the authoring generation. Tolerant of the usual
/// model noise (a think block, markdown bold, a leading bullet, a missing
/// prefix) because the cost of being strict is falling back to a worse question
/// for no reason.
fn parse_ask_line(text: &str) -> Option<String> {
    let cleaned = strip_think(text);
    for line in cleaned.lines() {
        let l = line.trim().trim_start_matches(['-', '*', '#', '>', ' ']).trim();
        let l = l.trim_start_matches("**").trim();
        let body = match l.strip_prefix("ASK:").or_else(|| l.strip_prefix("ask:")) {
            Some(rest) => rest.trim(),
            // a model that just wrote the question without the prefix has still
            // done the job, as long as the line looks like a question and not
            // like commentary about the task
            None if l.len() > 12 && l.ends_with('?') => l,
            None => continue,
        };
        // a model that bolded the prefix leaves the closing `**` on the body
        let body = body.trim().trim_start_matches("**").trim().trim_matches('"').trim();
        if body.len() >= 8 {
            return Some(body.chars().take(600).collect());
        }
    }
    None
}

/// Run the delegated vision leg, if this turn carries images and the deployment
/// configured a service, and fold the answer into the turn it belongs to.
///
/// The report goes in the USER turn rather than the system prompt for the same
/// reason search results do: it is data about one question, not standing
/// instruction, and build_prompt's overflow trimming drops the OLDEST turns -
/// so the report can never be evicted while the question it belongs to stays.
///
/// After this returns, NO message carries image bytes any more. That is the
/// point: the serving model is (usually) not a VLM, and an image left on a turn
/// would either be refused or fed to a model that cannot use it. Older turns
/// that carried pictures keep a one-line marker, so the model does not read a
/// bare "what about this one?" as a reference to nothing.
fn apply_vision(
    cfg: &AppConfig,
    creq: &ChatReq,
    messages: &mut [ChatMsg],
    tok: &Tokenizer,
    target_mode: &str,
    on_status: &dyn Fn(&str),
) -> Result<Option<VisionMeta>, String> {
    let Some(vcfg) = cfg.vision_service.clone() else { return Ok(None) };
    // the LAST turn carrying pictures is the one this request is about
    let Some(idx) = messages.iter().rposition(|m| !m.images.is_empty()) else { return Ok(None) };
    if vision_plan(cfg, creq.model.as_deref()) == VisionPlan::Local {
        return Ok(None);
    }

    let user_text = messages[idx].content.trim().to_string();
    let question = if vcfg.author_query {
        on_status("working out what to ask about the image…");
        author_vision_query(cfg, tok, messages, target_mode, on_status).unwrap_or_else(|| {
            if user_text.is_empty() {
                "Describe this image in detail, and transcribe any text exactly as it appears."
                    .to_string()
            } else {
                user_text.clone()
            }
        })
    } else if user_text.is_empty() {
        "Describe this image in detail, and transcribe any text exactly as it appears.".to_string()
    } else {
        user_text.clone()
    };

    let images = messages[idx].images.clone();
    on_status(&format!(
        "looking at the {} with the vision model…",
        if images.len() == 1 { "image" } else { "images" }
    ));
    let answer = vision::describe(&vcfg, &images, &question, None, || now_ms() as u64, on_status)?;

    // Drop every picture: nothing downstream can use the bytes now.
    for (i, m) in messages.iter_mut().enumerate() {
        if m.images.is_empty() || i == idx {
            continue;
        }
        let note = if m.images.len() == 1 {
            "[an image the user attached earlier in this conversation]"
        } else {
            "[images the user attached earlier in this conversation]"
        };
        m.content = if m.content.trim().is_empty() {
            note.to_string()
        } else {
            format!("{note}\n{}", m.content.trim())
        };
        m.images.clear();
    }
    messages[idx].images.clear();
    messages[idx].content = if user_text.is_empty() {
        format!(
            "{}\nThe user sent the image with no message. Tell them what it shows, in your own \
             words.",
            vision::render_context(&answer)
        )
    } else {
        format!("{}\nQuestion: {user_text}", vision::render_context(&answer))
    };

    Ok(Some(VisionMeta {
        model: answer.model,
        question: answer.question,
        images: answer.images,
        image_tokens: answer.image_tokens,
        ms: answer.ms,
    }))
}

struct SearchMeta {
    provider: String,
    /// (title, url) per hit, in the order the model was shown them, so the
    /// UI can render a citation list that matches the [n] markers
    sources: Vec<(String, String)>,
    ms: u64,
}

/// Run the web-search leg, if this request asked for one and the deployment
/// configured a provider, and fold the results into the LAST user turn.
///
/// The results go in the user turn rather than the system prompt on purpose.
/// Retrieved text is data about one question, not standing instruction: it
/// belongs next to the question it was fetched for, and putting it there also
/// means build_prompt's overflow trimming (which drops the OLDEST turns) can
/// never evict the results while keeping the question they belong to.
///
/// The query is the user's message verbatim. A model-written query would read
/// better, but it costs a whole extra generate() round trip in front of every
/// searched turn, and search engines are already built for the messy phrasing
/// people type. Worth revisiting if hit quality disappoints.
#[allow(clippy::too_many_arguments)]
fn apply_web_search(
    cfg: &AppConfig,
    creq: &ChatReq,
    messages: &mut Vec<ChatMsg>,
    tok: &Tokenizer,
    target_mode: &str,
    image_out: &mut Option<image::GeneratedImage>,
    // the reasoning rating, when this turn wants one: it rides OUT of here
    // because the router pass that decides about search answers both questions
    // at once, and paying for that pass twice would be the whole saving gone
    effort_out: &mut Option<Effort>,
    want_effort: bool,
    // the MODEL holds a web_search tool this turn, so the router must not
    // decide about search. Image routing is untouched: the two verdicts are
    // independent (see RouterAsk).
    model_searches: bool,
    on_status: &dyn Fn(&str),
) -> Result<Option<SearchMeta>, String> {
    let web_mode = creq.web_mode();
    let Some(last) = messages.iter().rposition(|m| m.role == "user") else {
        if web_mode == WebMode::Always {
            return Err("no user message to search for".into());
        }
        return Ok(None);
    };
    // A `/search ` or `/image ` prefix is a per-turn request, independent of
    // any client-side switch: it travels with the ONE message that wanted it,
    // so it cannot leak into the next turn the way a sticky toggle does, and
    // it works for API clients that have no UI to toggle.
    let stripped = strip_search_prefix(&messages[last].content);
    let asked_inline = stripped.is_some();
    let inline_image = strip_image_prefix(&messages[last].content);
    // TWO capabilities, TWO gates. They used to share one switch, so a user who
    // did not want their questions going to a search provider also lost image
    // generation - a service this operator runs themselves, with an entirely
    // different disclosure. `image_configured` is the DEPLOYMENT'S gate, which
    // an explicit /image answers to alone because typing the command is the
    // consent; `image_live` is this turn's, and it is what the router sees.
    let image_configured = cfg.image.is_some();
    let image_live = image_configured
        && creq.image_on(cfg.image.as_ref().map(|i| i.default_on).unwrap_or(false));

    // an explicit /image bypasses the router entirely
    if let Some(prompt) = inline_image {
        messages[last].content = prompt.clone();
        if !image_configured {
            return Ok(None); // no service: answer normally rather than fail
        }
        on_status("generating the image…");
        return run_image(cfg, &prompt, messages, last, image_out, on_status).map(|()| None);
    }

    if let Some(rest) = stripped {
        // the model must never see the command word - it is UI, not content
        messages[last].content = rest;
    }
    // ONE router pass, asked only about the capabilities live for this turn:
    // two passes would be two generations of latency to answer one question,
    // and offering a verdict the turn cannot act on is how you get an IMAGE
    // back on a turn whose user switched images off.
    let router_search = web_mode == WebMode::Auto && !model_searches;
    if !asked_inline && web_mode != WebMode::Always && (router_search || image_live) {
        on_status("deciding what this needs…");
        let ask = RouterAsk { search: router_search, image: image_live, effort: want_effort };
        let out = route_web_search(cfg, tok, messages, target_mode, ask, on_status);
        *effort_out = out.effort;
        match out.verdict {
            Some(RouterVerdict::Image(prompt)) => {
                on_status("generating the image…");
                return run_image(cfg, &prompt, messages, last, image_out, on_status).map(|()| None);
            }
            Some(RouterVerdict::Search(q)) => {
                if cfg.search.is_none() {
                    return Ok(None);
                }
                on_status("searching the web…");
                return finish_search(cfg, messages, last, q, web_mode, asked_inline, on_status);
            }
            None => return Ok(None),
        }
    }
    // nothing left to do unless the turn asked for a search outright
    if !asked_inline && web_mode != WebMode::Always {
        return Ok(None);
    }
    let Some(_) = &cfg.search else {
        // no provider configured: an inline /search should not eat the turn.
        // Only an explicit web_search:true, which asked for something we
        // cannot do, is an error.
        if web_mode != WebMode::Always {
            return Ok(None);
        }
        return Err("web search is not enabled on this deployment".into());
    };
    let query = messages[last].content.trim().to_string();
    on_status("searching the web…");
    finish_search(cfg, messages, last, query, web_mode, asked_inline, on_status)
}

/// Generate an image and fold a note about it into the user's turn, so the
/// model writes a reply that acknowledges the picture it cannot see.
///
/// A failure here is returned to the caller only when the user ASKED for an
/// image; the auto path treats it the way it treats a dead search provider.
fn run_image(
    cfg: &AppConfig,
    prompt: &str,
    messages: &mut [ChatMsg],
    last: usize,
    image_out: &mut Option<image::GeneratedImage>,
    on_status: &dyn Fn(&str),
) -> Result<(), String> {
    let icfg = cfg.image.as_ref().ok_or("image generation is not enabled on this deployment")?;
    let img = image::generate(icfg, prompt, || now_ms() as u64, on_status)?;
    // The model never sees the bytes - it is a text model. It is told the
    // image exists and what was asked for, which is enough to write "here is
    // the fox you asked for" instead of describing a picture it invented.
    messages[last].content = format!(
        "{}\n\n[An image has ALREADY been generated for this request and is \
         displayed to the user directly above your reply. It was generated from \
         the prompt: \"{}\". Do not describe the image in detail - you cannot \
         see it. Acknowledge it briefly and naturally, and offer to adjust it.]",
        messages[last].content.trim(),
        img.prompt
    );
    *image_out = Some(img);
    Ok(())
}

/// Tell the model a search the user wanted did not happen, in the turn it
/// belongs to. Same bracketed-note pattern as run_image: a fact about this
/// request, travelling with the message rather than rewriting the system
/// prompt. Without it the model answers as if the web had never been in play,
/// and the user who flipped the switch is misled by an answer that LOOKS
/// checked.
fn note_failed_search(messages: &mut [ChatMsg], last: usize) {
    messages[last].content = format!(
        "{}\n\n[Web search was attempted for this request but FAILED: no results were \
         retrieved. Answer from your own knowledge, and say plainly that you could not \
         check the web and the answer is unverified. Do not invent sources or citations.]",
        messages[last].content.trim()
    );
}

fn finish_search(
    cfg: &AppConfig,
    messages: &mut [ChatMsg],
    last: usize,
    query: String,
    web_mode: WebMode,
    asked_inline: bool,
    on_status: &dyn Fn(&str),
) -> Result<Option<SearchMeta>, String> {
    let scfg = cfg.search.as_ref().ok_or("web search is not enabled on this deployment")?;
    let t0 = now_ms();
    let hits = match search::search(scfg, &query) {
        Ok(h) => h,
        // A retrieval that failed must not take the answer down with it - in
        // ANY mode. Auto never asked, so it degrades silently; a turn that DID
        // ask (the switch, /search) gets the model told instead, so the reply
        // says plainly that the web could not be checked rather than the whole
        // request dying on a flaky egress. The real error goes to the log and
        // the /search?q= probe still surfaces it for the operator.
        Err(e) => {
            eprintln!("[llm-chat] web search failed, answering without it: {e}");
            if web_mode != WebMode::Auto || asked_inline {
                on_status("search failed; answering from model knowledge…");
                note_failed_search(messages, last);
            }
            return Ok(None);
        }
    };
    if hits.is_empty() {
        if web_mode != WebMode::Auto || asked_inline {
            on_status("no results; answering from model knowledge…");
            note_failed_search(messages, last);
        }
        return Ok(None);
    }
    let sources = hits.iter().map(|h| (h.title.clone(), h.url.clone())).collect();
    // The question put back to the model is the USER'S, not the router's
    // search query - those differ in auto mode, and answering the query
    // instead of the question is how you get a reply about the wrong thing.
    let question = messages[last].content.trim().to_string();
    messages[last].content = format!(
        "{}\nQuestion: {question}",
        search::render_context(&query, &hits)
    );
    Ok(Some(SearchMeta {
        provider: scfg.provider.clone(),
        sources,
        ms: now_ms().saturating_sub(t0) as u64,
    }))
}

/// `/search <query>` or `/web <query>` at the head of a message: returns the
/// message with the command removed. Case-insensitive, and the command must
/// be followed by whitespace and something to search for, so a message that
/// merely BEGINS with the word (or asks about "/search" itself) is untouched.
fn strip_image_prefix(content: &str) -> Option<String> {
    strip_cmd_prefix(content, &["/image", "/img", "/draw"])
}

fn strip_search_prefix(content: &str) -> Option<String> {
    strip_cmd_prefix(content, &["/search", "/web"])
}

fn strip_cmd_prefix(content: &str, cmds: &[&str]) -> Option<String> {
    let t = content.trim_start();
    for cmd in cmds {
        if t.len() > cmd.len() && t[..cmd.len()].eq_ignore_ascii_case(cmd) {
            let rest = &t[cmd.len()..];
            if rest.starts_with(char::is_whitespace) && !rest.trim().is_empty() {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

fn search_meta_json(m: &SearchMeta) -> serde_json::Value {
    serde_json::json!({
        "provider": m.provider,
        "ms": m.ms,
        "sources": m.sources.iter()
            .map(|(t, u)| serde_json::json!({ "title": t, "url": u }))
            .collect::<Vec<_>>(),
    })
}

fn read_body(req: &IncomingRequest) -> Result<Vec<u8>, String> {
    let body = req.consume().map_err(|_| "request has no body")?;
    let stream = body.stream().map_err(|_| "cannot read request body")?;
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if out.len() > MAX_BODY_BYTES {
                    return Err("request body too large".into());
                }
            }
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("body read error: {e:?}")),
        }
    }
    Ok(out)
}

/// Reasoning is per-turn scratch: a replayed assistant turn keeps only what
/// followed its think block, exactly as the qwen3.x templates render history
/// (split on the last `</think>`, strip leading newlines).
fn strip_think(content: &str) -> String {
    match content.rfind("</think>") {
        Some(i) => content[i + "</think>".len()..].trim_start_matches('\n').to_string(),
        None => content.to_string(),
    }
}

/// One run of a prompt: either tokens, or a picture the HOST has to encode.
enum PromptPart {
    Text(Vec<u32>),
    /// raw image file bytes, handed to the host's "image" verb verbatim
    Image(Vec<u8>),
}

/// A prompt ready to feed: its parts in order, plus the flat token stream for
/// everything that IS text. A text-only prompt is exactly what it always was,
/// which is what lets every existing path (speculative decode, the repetition
/// window, the token stats) keep working untouched.
struct Prompt {
    parts: Vec<PromptPart>,
    text_ids: Vec<u32>,
    images: usize,
}

impl Prompt {
    /// The whole prompt as tokens, or None when a picture is in the way. The
    /// paths that cannot express an image (speculative decode: its position
    /// bookkeeping counts tokens, and an image advances positions by its own
    /// arithmetic) ask for this and fall back to the plain path on None.
    fn text_only(&self) -> Option<&[u32]> {
        (self.images == 0).then_some(self.text_ids.as_slice())
    }

}

/// Render + tokenize the conversation; drops oldest turns until it fits.
/// A `system` message in the request overrides the configured default.
/// The returned bool is Rendered::think_open: the prompt force-opened a
/// think block that the caller must re-emit in the visible output.
///
/// IMAGES: an attachment leaves a MEDIA_MARK in its turn's text, the template
/// renders the conversation as usual, and the result is split back apart on
/// those marks. So the model receives its own chat format exactly as it was
/// trained on it, with the pictures sitting where the marks were, and this
/// function needs to know nothing about how any particular VLM wraps an image
/// (the host's projector adds whatever tokens the model expects around it).
/// State the model's ACTUAL capabilities at the end of the system prompt.
///
/// This app has no tool-calling API. Web search is a decision made BEFORE the
/// answer starts (see route_web_search) and the results arrive inside the user's
/// turn, so a model instructed to "call a web search tool" has no way to comply
/// and does the only thing left: it writes something that looks like a tool
/// call. Reported 2026-07-30 as a reply of nothing but
/// `<tool_code>search_tool(query="…")</tool_code>`, which is not a bug in the
/// model so much as an unanswered question about what it can do.
///
/// So the prompt answers it. Appended rather than prepended: a deployment's own
/// system prompt is what sets the assistant's character, and this is a footnote
/// about the machinery, not a competing instruction. Only when the deployment
/// actually has a search leg - a model told about a capability nobody
/// configured would be worse off than one told nothing.
/// What the system prompt tells the model about what it can do this turn.
#[derive(Clone, Copy)]
enum Capabilities<'a> {
    /// this app's own internal passes (router, title, vision query): say
    /// nothing. A pass asked to rate a turn does not need to be told it cannot
    /// browse, and a tools block would invite it to answer with a call.
    Internal,
    /// the answer at a deployment with no tools: the "you cannot call anything"
    /// note, which is what stops a model from writing a fake tool call
    Note,
    /// the answer with a tool registry: the real signatures, and the stop
    /// string that ends generation the moment a call is complete
    Tools(&'a [tools::Tool], usize),
    /// the answer with CLIENT-declared tools (the /v1 passthrough): the block
    /// is pre-rendered by the handler (see tools::client_system_block), and
    /// the same stop string arms - a completed call ends the turn, because
    /// executing it is the client's job
    Client(&'a str),
}

fn with_capability_note(cfg: &AppConfig, system: &str) -> String {
    if cfg.search.is_none() {
        return system.to_string();
    }
    let note = "How this app works, which overrides any instruction to the contrary: you have NO \
tools and cannot call one. You cannot search, browse, run code, or fetch a URL yourself. When a \
turn needs the web, this app decides that BEFORE you are asked and the results are already in the \
user's message under \"Web results\", with numbered sources to cite as [1], [2]. If there are no \
results in the message, none were fetched: answer from your own knowledge and say plainly that it \
is unverified. NEVER write a tool call, a function call, or a code block that pretends to search - \
nothing executes it, and the user sees the fake call instead of an answer.";
    if system.trim().is_empty() {
        return note.to_string();
    }
    format!("{}\n\n{note}", system.trim_end())
}

fn build_prompt(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
    thinking: bool, // the request's switch; only cfg.thinking models act on it
    caps: Capabilities,
) -> Result<(Prompt, Vec<String>, bool), String> {
    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| strip_marks(&m.content))
        .unwrap_or_else(|| cfg.system_prompt.clone());
    let system = match caps {
        Capabilities::Internal => system,
        Capabilities::Note => with_capability_note(cfg, &system),
        // the tools block REPLACES the note rather than joining it: the note
        // says "you have no tools and cannot call one", which is a lie at a
        // deployment that just handed the model three of them
        Capabilities::Tools(list, max) => {
            format!("{}{}", system.trim_end(), tools::system_block(list, max))
        }
        Capabilities::Client(block) => format!("{}{}", system.trim_end(), block),
    };
    // (role, text) per turn, with the turn's images kept alongside by INDEX.
    // Marks are stripped from incoming text FIRST: they are our own private
    // punctuation, and a message that arrived carrying one must not be able to
    // claim an image slot.
    let mut msgs: Vec<(String, String)> = Vec::new();
    let mut turn_images: Vec<&[Vec<u8>]> = Vec::new();
    for m in messages.iter().filter(|m| m.role == "user" || m.role == "assistant") {
        let content = if cfg.thinking && m.role == "assistant" {
            strip_think(&m.content)
        } else {
            m.content.clone()
        };
        let mut content = strip_marks(&content);
        // Pictures lead the turn, then the words about them. An image-only
        // turn still gets a sentence: a bare image with no instruction is
        // out of distribution for chat-tuned VLMs, and "describe it" is
        // what a user who typed nothing meant.
        if !m.images.is_empty() {
            let marks = config::MEDIA_MARK.repeat(m.images.len());
            content = if content.trim().is_empty() {
                format!("{marks}\nDescribe this image in detail.")
            } else {
                format!("{marks}\n{content}")
            };
        }
        msgs.push((m.role.clone(), content));
        turn_images.push(&m.images);
    }
    if msgs.is_empty() {
        return Err("no user/assistant messages".into());
    }
    let think = if !cfg.thinking {
        config::ThinkTurn::Plain
    } else if thinking {
        config::ThinkTurn::Open
    } else {
        config::ThinkTurn::Closed
    };
    loop {
        let rendered = config::render_template(&cfg.template, &system, &msgs, think)?;
        // The picture bytes are cloned ONCE, for the render that actually
        // fits: a trim round that is about to be thrown away has no business
        // copying several megabytes to find that out.
        let images: usize = turn_images.iter().map(|im| im.len()).sum();
        let total = tokens_of(tok, &rendered.prompt)? + images * cfg.image_tokens;
        if total <= cfg.max_prompt_tokens || msgs.len() <= 1 {
            if total > cfg.max_prompt_tokens {
                return Err(format!(
                    "message too long: {total} tokens (limit {}){}",
                    cfg.max_prompt_tokens,
                    if images > 0 {
                        format!(" - {images} image(s) are budgeted at {} tokens each",
                                cfg.image_tokens)
                    } else {
                        String::new()
                    }
                ));
            }
            let bytes: Vec<Vec<u8>> =
                turn_images.iter().flat_map(|im| im.iter().cloned()).collect();
            let prompt = split_rendered(tok, &rendered.prompt, bytes)?;
            let mut stops = rendered.stop_strings;
            // A completed call is the end of the turn: stopping here saves the
            // model from narrating past its own call, and the parser accepts
            // the unterminated form the stop string leaves behind.
            if matches!(caps, Capabilities::Tools(..) | Capabilities::Client(_)) {
                stops.push("</tool_call>".into());
            }
            return Ok((prompt, stops, rendered.think_open));
        }
        msgs.remove(0); // drop the oldest turn and retry
        turn_images.remove(0); // ...and the pictures that were part of it
    }
}

/// How many tokens a rendered prompt costs, media marks excluded (they are
/// punctuation for the splitter, not text for the model).
fn tokens_of(tok: &Tokenizer, rendered: &str) -> Result<usize, String> {
    let text = rendered.replace(config::MEDIA_MARK, "");
    Ok(tok.encode(text.as_str(), true).map_err(|e| format!("tokenize: {e}"))?.len())
}

/// Cut the rendered prompt at its media marks and tokenize the text between
/// them. Only the FIRST run gets the tokenizer's special-token treatment, the
/// same as a whole prompt would: later runs continue a sequence that already
/// began, so re-adding a BOS at each image would corrupt it.
fn split_rendered(
    tok: &Tokenizer,
    rendered: &str,
    images: Vec<Vec<u8>>,
) -> Result<Prompt, String> {
    let chunks: Vec<&str> = rendered.split(config::MEDIA_MARK).collect();
    if chunks.len() - 1 != images.len() {
        return Err(format!(
            "prompt has {} image slots but {} images",
            chunks.len() - 1,
            images.len()
        ));
    }
    let mut parts = Vec::with_capacity(chunks.len() + images.len());
    let mut text_ids = Vec::new();
    let mut imgs = images.into_iter();
    for (i, chunk) in chunks.iter().enumerate() {
        if !chunk.is_empty() {
            let enc = tok
                .encode(*chunk, i == 0)
                .map_err(|e| format!("tokenize: {e}"))?;
            let ids = enc.get_ids().to_vec();
            if !ids.is_empty() {
                text_ids.extend_from_slice(&ids);
                parts.push(PromptPart::Text(ids));
            }
        }
        if let Some(img) = imgs.next() {
            parts.push(PromptPart::Image(img));
        }
    }
    let images = parts.iter().filter(|p| matches!(p, PromptPart::Image(_))).count();
    Ok(Prompt { parts, text_ids, images })
}

/// Remove media marks from text that came from outside.
fn strip_marks(s: &str) -> String {
    if s.contains(config::MEDIA_MARK) {
        return s.replace(config::MEDIA_MARK, "");
    }
    s.to_string()
}

/// The reasoning rating for this turn, and the pass that produces one when the
/// web router did not already run. Returns None when the deployment has not
/// configured effort scaling, when the turn opens no reasoning block, or when
/// the model declines to rate it - each of which leaves the flat think_budget
/// in charge, which is the behaviour every deployment had before this existed.
fn resolve_effort(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
    mode: &str,
    think_open: bool,
    from_router: Option<Effort>,
    on_status: &dyn Fn(&str),
) -> Option<Effort> {
    if cfg.effort.is_none() || !think_open {
        return None;
    }
    if from_router.is_some() {
        return from_router; // already paid for, in the router's own pass
    }
    on_status("sizing the reasoning budget…");
    route_web_search(
        cfg,
        tok,
        messages,
        mode,
        RouterAsk { search: false, image: false, effort: true },
        on_status,
    )
    .effort
}

/// `think_open` is build_prompt's: the think budget only arms on a turn whose
/// prompt actually force-opened a reasoning block, which is what makes it
/// inert for non-thinking models and for enable_thinking=false turns.
///
/// `effort` is the turn's rating, when the deployment scales the budget by it.
fn gen_params(
    cfg: &AppConfig,
    creq: &ChatReq,
    extra_stops: Vec<String>,
    think_open: bool,
    effort: Option<Effort>,
) -> GenParams {
    let mut stops = extra_stops;
    match &creq.stop {
        Some(serde_json::Value::String(s)) if !s.is_empty() => stops.push(s.clone()),
        Some(serde_json::Value::Array(a)) => {
            for v in a.iter().take(4) {
                if let Some(s) = v.as_str() {
                    stops.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    GenParams {
        max_new: creq
            .max_tokens
            .or(creq.max_completion_tokens)
            .unwrap_or(cfg.default_max_new)
            .min(cfg.max_new_cap)
            .max(1),
        sample: SampleParams {
            temperature: creq.temperature.unwrap_or(cfg.temperature).clamp(0.0, 2.0),
            top_p: creq.top_p.unwrap_or(cfg.top_p).clamp(0.05, 1.0),
            top_k: creq.top_k.unwrap_or(cfg.top_k),
            rep_penalty: cfg.rep_penalty,
            rep_window: cfg.rep_window,
        },
        stop_strings: stops,
        think_budget: match effort {
            Some(e) => e.budget(cfg),
            None => cfg.think_budget,
        },
        think_open,
        loop_reps: cfg.repeat_guard,
    }
}

fn targets_for(cfg: &AppConfig, mode: &str) -> Vec<(ExecutionTarget, &'static str)> {
    // ggml ignores the execution target entirely (the model is host-preloaded,
    // device offload is the node env's call), so auto mode's CPU retry would
    // just repeat the SAME failed attempt relabeled "cpu:" — the misleading
    // "cpu: llama.cpp context allocation failed…" users saw live 2026-07-24.
    // One attempt, honestly labeled - and the LABEL follows the deployment:
    // with no GPU share there is nothing to offload to, so the host runs the
    // graph on CPU and saying "gpu" would be a lie in the status line.
    if cfg.backend == "ggml" {
        let label = if gpu_present() == Some(false) { "cpu" } else { "gpu" };
        return vec![(ExecutionTarget::Gpu, label)];
    }
    match mode {
        "cpu" => vec![(ExecutionTarget::Cpu, "cpu")],
        "gpu" => vec![(ExecutionTarget::Gpu, "gpu")],
        _ => vec![(ExecutionTarget::Gpu, "gpu"), (ExecutionTarget::Cpu, "cpu")],
    }
}

fn respond_bytes(out: ResponseOutparam, status: u16, ctype: &str, body_bytes: &[u8]) {
    respond_with_cache(out, status, ctype, body_bytes, None)
}

/// Static assets get a long immutable cache: the font never changes within a
/// published version, and a redeploy serves from a new origin anyway.
fn respond_asset(out: ResponseOutparam, ctype: &str, body_bytes: &[u8]) {
    respond_with_cache(out, 200, ctype, body_bytes, Some("public, max-age=31536000, immutable"))
}

/// Public, cross-origin-readable JSON. The attestation document and the
/// /.well-known files exist for OTHER parties' verifiers - the mobile shell's
/// bundled splash, Google/Apple link validators, anyone's monitor - so they
/// carry an open CORS header; no-cache because both are how a stable custom
/// domain announces its CURRENT state.
fn respond_public_json(out: ResponseOutparam, status: u16, body_bytes: &[u8]) {
    respond_full(out, status, "application/json", body_bytes, Some("no-cache"), true)
}

fn respond_with_cache(
    out: ResponseOutparam,
    status: u16,
    ctype: &str,
    body_bytes: &[u8],
    cache: Option<&str>,
) {
    respond_full(out, status, ctype, body_bytes, cache, false)
}

fn respond_full(
    out: ResponseOutparam,
    status: u16,
    ctype: &str,
    body_bytes: &[u8],
    cache: Option<&str>,
    cors: bool,
) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
    if let Some(c) = cache {
        let _ = headers.set(&"cache-control".to_string(), &[c.as_bytes().to_vec()]);
    }
    if cors {
        let _ = headers.set(&"access-control-allow-origin".to_string(), &[b"*".to_vec()]);
    }
    let resp = OutgoingResponse::new(headers);
    let _ = resp.set_status_code(status);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    // the platform caps a single body write at 4096 bytes
    for chunk in body_bytes.chunks(4000) {
        if stream.blocking_write_and_flush(chunk).is_err() {
            break;
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

/// Machine-readable conditions ggml_load_err() tags inside its messages (as
/// "[code] "); json_err lifts the tag into `error.code` so the playground can
/// render the right state instead of pattern-matching prose.
const ERR_CODES: &[&str] = &[
    "model_not_loaded", "host_load_failed", "volume_not_attached", "sessions_busy",
    // vision: the first three are the user's to fix (wrong model, too many,
    // too big), the rest describe the deployment or its node
    "no_vision", "too_many_images", "image_too_large", "vision_unsupported",
    "vision_unavailable", "image_undecodable", "image_too_wide",
];

/// Drop the "[code] " marker from a message bound for a payload without a
/// code field of its own (the chat stream / SSE error lines).
fn strip_code(msg: &str) -> String {
    let mut m = msg.to_string();
    for c in ERR_CODES {
        m = m.replacen(&format!("[{c}] "), "", 1);
    }
    m
}

fn json_err(out: ResponseOutparam, status: u16, msg: &str) {
    let code = ERR_CODES.iter().copied().find(|c| msg.contains(&format!("[{c}] ")));
    let msg = strip_code(msg);
    let mut err = serde_json::json!({ "message": msg, "type": "invalid_request_error" });
    if let Some(c) = code {
        err["code"] = serde_json::json!(c);
    }
    respond_bytes(
        out,
        status,
        "application/json",
        serde_json::json!({ "error": err }).to_string().as_bytes(),
    );
}

/// Bearer-token check for /v1/*. No key configured = open (gate with a
/// private deployment instead when that is the intent).
fn authorized(cfg: &AppConfig, req: &IncomingRequest) -> bool {
    let Some(key) = &cfg.api_key else { return true };
    let headers = req.headers();
    for v in headers.get(&"authorization".to_string()) {
        if let Ok(s) = String::from_utf8(v) {
            if let Some(tok) = s.strip_prefix("Bearer ") {
                if tok.trim() == key {
                    return true;
                }
            }
        }
    }
    false
}

// ------------------------------------------------- legacy /chat (playground) --

/// Name a chat from its opening exchange. Takes the same body shape as /chat
/// (a `messages` array) and reads only the first question and the first reply.
///
/// Open, like /chat and /models: it is the playground naming its own sidebar
/// entry, and it can do nothing a /chat turn could not already do.
///
/// A failure is not an error here - it is a 200 with `title: null`, and the
/// caller keeps whatever placeholder it already showed. Nothing about naming a
/// conversation is worth an error message in front of a user.
fn handle_title(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = &match resolve_model(raw, creq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    let question = creq
        .messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| {
            // a picture-only opener still names something
            if m.content.trim().is_empty() && !m.images.is_empty() {
                format!("[{} image(s), no question]", m.images.len())
            } else {
                m.content.clone()
            }
        })
        .unwrap_or_default();
    if question.trim().is_empty() {
        return json_err(out, 400, "no user message to name the chat from");
    }
    let answer = creq
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let title = read_tokenizer(cfg)
        .ok()
        .and_then(|b| Tokenizer::from_bytes(&b).ok())
        .and_then(|tok| make_title(cfg, &tok, &question, &answer, "auto"));
    let body = serde_json::json!({ "title": title });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

fn handle_chat(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = &match resolve_model(raw, creq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    // Attachments are vetted BEFORE the stream opens: this is the one class of
    // failure where a plain HTTP status is better than an error event, because
    // the client can then keep the picture and let the user pick another model
    // rather than treating it as a failed turn.
    if let Err(e) = check_images(raw, cfg, &creq.messages) {
        return json_err(out, 400, &e);
    }
    let tok_bytes = match read_tokenizer(cfg) {
        Ok(b) => b,
        Err(e) => return json_err(out, 500, &e),
    };
    let tok = match Tokenizer::from_bytes(&tok_bytes) {
        Ok(t) => t,
        Err(e) => return json_err(out, 500, &format!("tokenizer: {e}")),
    };
    // The SSE stream opens BEFORE the search leg, because in auto mode that
    // leg can run a router generation and then a provider round trip - several
    // seconds in which a silent "Preparing…" is all the user would see. With
    // the stream open first, each step narrates itself, which is the whole
    // difference between "thinking about it" and "apparently hung". The cost
    // is that failures here are error EVENTS rather than HTTP status codes;
    // the playground renders both the same way, and /v1 keeps the status codes.
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    let send = |v: serde_json::Value| -> bool {
        let msg = format!("data: {v}\n\n");
        for chunk in msg.as_bytes().chunks(4000) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return false;
            }
        }
        true
    };

    let mode = creq.target.as_deref().unwrap_or("auto");

    // --- web search leg, narrated ---
    let mut messages = creq.messages.clone();
    let mut generated_image: Option<image::GeneratedImage> = None;
    let status_cb = |s: &str| {
        let _ = send(serde_json::json!({ "status": s }));
    };
    // Effort scaling, when the deployment configured it: the rating comes out
    // of the router pass below if that runs, and from its own short pass later
    // if it does not. `want_effort` gates BOTH, so a deployment without the
    // block pays for neither.
    let mut router_effort: Option<Effort> = None;
    let want_effort = cfg.effort.is_some() && cfg.thinking && creq.thinking();
    // --- tools, resolved FIRST. Two reasons for the order: the schemas have to
    // be in the prompt, and a deployment that gave the model a web_search tool
    // must stop the router deciding about search, or the turn pays for a
    // provider round trip the model never asked for and then gets the tool too.
    let mut tl = tools_enabled(cfg, &creq)
        .map(|tc| ToolLoop::open(tc, builtins_for(cfg, &creq), &status_cb));
    if let Some(t) = &tl {
        for n in &t.reg.notes {
            let _ = send(serde_json::json!({ "notice": format!("tools: {n}") }));
        }
    }
    if tl.as_ref().is_some_and(|t| !t.armed()) {
        tl = None;
    }
    let model_searches = tl.as_ref().is_some_and(|t| t.owns_search());
    let search_meta = match apply_web_search(
        cfg,
        &creq,
        &mut messages,
        &tok,
        mode,
        &mut generated_image,
        &mut router_effort,
        want_effort,
        model_searches,
        &status_cb,
    ) {
        Ok(m) => m,
        Err(e) => {
            let _ = send(serde_json::json!({ "error": format!("{e}") }));
            drop(stream);
            let _ = OutgoingBody::finish(body, None);
            return;
        }
    };
    // tell the client what was read so the sources can be shown while the
    // model is still working through them
    if let Some(m) = &search_meta {
        let _ = send(serde_json::json!({ "search": search_meta_json(m) }));
    }
    // --- vision leg, narrated. AFTER the search leg so a "/search ..." prefix
    // is still at the head of the message where strip_search_prefix looks for
    // it; the report is prepended to whatever that leg left behind.
    let vision_meta = match apply_vision(cfg, &creq, &mut messages, &tok, mode, &status_cb) {
        Ok(m) => m,
        Err(e) => {
            let _ = send(serde_json::json!({ "error": format!("{e}") }));
            drop(stream);
            let _ = OutgoingBody::finish(body, None);
            return;
        }
    };
    if let Some(m) = &vision_meta {
        let _ = send(serde_json::json!({ "vision": vision_meta_json(m) }));
    }
    // the image lands BEFORE the reply, so it is on screen while the model
    // writes its sentence about it
    if let Some(img) = &generated_image {
        let _ = send(serde_json::json!({
            "image": {
                "data_uri": img.data_uri(),
                "prompt": img.prompt,
                "model": img.model,
                "seed": img.seed,
                "ms": img.ms,
            }
        }));
    }
    let (ref draft_cfg, draft_note) = resolve_draft(raw, cfg);
    if let Some(n) = &draft_note {
        let _ = send(serde_json::json!({ "status": format!("speculative decode off: {n}") }));
    }
    // One retry in hand for a fabricated tool call: a model told to "call a
    // search tool" can write one anyway, and this app has no tool to call. If
    // the turn is allowed to reach the web, the search it asked for is run for
    // real and the answer regenerates from the results. Only with the user's
    // web switch on: a fake tool call is not consent to send their question to
    // a provider. See fabricated_tool_query.
    // Two remedies, each usable once: fetch what it asked for, and then, if
    // it writes ANOTHER call with the results already in front of it, tell it
    // plainly that there are no tools. A prompt emphatic enough to keep
    // faking calls is the user's to fix; two passes is where this app stops.
    let (mut tool_searched, mut tool_nudged) = (false, false);
    let may_search = cfg.search.is_some() && creq.web_mode() != WebMode::Off;
    let last_user = messages.iter().rposition(|m| m.role == "user");
    let mut last_err = String::new();
    let mut ok = false;
    'answer: loop {
        let caps = match &tl {
            Some(t) => Capabilities::Tools(t.tools(), t.cfg.max_calls),
            None => Capabilities::Note,
        };
        let (prompt_ids, stops, think_open) = match build_prompt(cfg, &tok, &messages, creq.thinking(), caps) {
            Ok(v) => v,
            Err(e) => {
                let _ = send(serde_json::json!({ "error": e }));
                drop(stream);
                let _ = OutgoingBody::finish(body, None);
                return;
            }
        };
        let effort = resolve_effort(cfg, &tok, &messages, mode, think_open, router_effort, &status_cb);
        let params = gen_params(cfg, &creq, stops, think_open, effort);
        for (i, (target, tname)) in targets_for(cfg, mode).iter().enumerate() {
            if i > 0 && !send(serde_json::json!({ "notice": format!("gpu failed ({last_err}); retrying on cpu") })) {
                break;
            }
            // the prompt force-opened the think block; re-emit the tag ahead of
            // the first real delta so the client sees a complete block. Lazy,
            // per attempt: a retry notice resets the client's reply buffer, and
            // an attempt that dies before producing output must not leak a tag.
            let opened = std::cell::Cell::new(!think_open);
            // a call is held back rather than shown and then retracted
            let gate = std::cell::RefCell::new(CallGate::new(tl.is_some(), think_open));
            let emit = |delta: &str| {
                let Some(out) = gate.borrow_mut().push(delta) else { return true };
                if !opened.replace(true) && !send(serde_json::json!({ "delta": "<think>\n" })) {
                    return false;
                }
                send(serde_json::json!({ "delta": out }))
            };
            let status = |s: &str| send(serde_json::json!({ "status": s }));
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &emit, &status) {
                Ok(s) => {
                    // the model asked for a tool this deployment actually has:
                    // run it, put the result in the conversation, answer again
                    if let Some(t) = &mut tl {
                        let on_call = |c: &serde_json::Value| {
                            let _ = send(serde_json::json!({ "tool": c }));
                        };
                        let on_result = |r: &serde_json::Value| {
                            let _ = send(serde_json::json!({ "tool_result": r }));
                            // a web_search the MODEL asked for feeds the same
                            // numbered source list a routed search does, so the
                            // citations in its answer resolve to something
                            if let Some(src) = r.get("sources") {
                                let _ = send(serde_json::json!({ "search": {
                                    "provider": cfg.search.as_ref()
                                        .map(|s| s.provider.clone()).unwrap_or_default(),
                                    "sources": src,
                                    "ms": r.get("ms").cloned().unwrap_or(serde_json::json!(0)),
                                } }));
                            }
                        };
                        if t.step(&s.text, &mut messages, &on_call, &on_result) {
                            continue 'answer;
                        }
                    }
                    // nothing ran: whatever the gate is still holding is the
                    // answer, and the user has been waiting for it
                    if let Some(rest) = gate.borrow_mut().flush() {
                        if !opened.replace(true) {
                            let _ = send(serde_json::json!({ "delta": "<think>\n" }));
                        }
                        let _ = send(serde_json::json!({ "delta": rest }));
                    }
                    // the model wrote a tool call at a deployment with no tools:
                    // run the search it wanted, then answer again from the results
                    if tl.is_none() {
                    if let Some(q) = fabricated_tool_query(&s.text) {
                        // the web is open and this is the first call: run the
                        // search it asked for, answer again from the results
                        if may_search && !tool_searched {
                            if let Some(li) = last_user {
                                tool_searched = true;
                                let _ = send(serde_json::json!({ "notice": format!(
                                    "the model asked to search the web for \"{}\"; running that \
                                     search and answering again",
                                    truncate_for_msg_n(&q, 120)) }));
                                let _ = send(serde_json::json!({ "status": "searching the web…" }));
                                // Always, not Auto: the model asked for this one
                                // by name, so a provider that fails must be
                                // owned up to - finish_search folds the
                                // could-not-check note into the turn and the
                                // regenerated answer says so
                                let leg = |s: &str| {
                                    let _ = send(serde_json::json!({ "status": s }));
                                };
                                match finish_search(cfg, &mut messages, li, q, WebMode::Always, true, &leg) {
                                    Ok(Some(m)) => {
                                        let _ = send(serde_json::json!({ "search": search_meta_json(&m) }));
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        let _ = send(serde_json::json!({ "notice": format!(
                                            "that search failed ({e}); answering without it") }));
                                    }
                                }
                                continue 'answer;
                            }
                        }
                        // either the web is closed to this turn - a fabricated
                        // call is not consent to send someone's question to a
                        // provider - or the results are already in front of it
                        // and it faked a call anyway. Say so and make it answer.
                        if !tool_nudged {
                            tool_nudged = true;
                            let _ = send(serde_json::json!({ "notice": if tool_searched {
                                "the model wrote another tool call with the results already in \
                                 front of it; answering from those results".to_string()
                            } else {
                                "the model tried to call a search tool, which this deployment does \
                                 not give it; answering from the model's own knowledge (turn on web \
                                 search to let it look things up)".to_string()
                            } }));
                            no_tools_nudge(cfg, &mut messages, tool_searched);
                            continue 'answer;
                        }
                    }
                    }
                    let gen_s = (s.decode_ms as f64) / 1000.0;
                    let tok_per_s = if gen_s > 0.0 { s.tokens as f64 / gen_s } else { 0.0 };
                    let mut done = serde_json::json!({
                        "done": true, "target": s.target,
                        "prompt_tokens": s.prompt_tokens, "tokens": s.tokens,
                        "load_ms": s.load_ms as u64, "prefill_ms": s.prefill_ms as u64,
                        "decode_ms": s.decode_ms as u64,
                        "finish_reason": s.finish_reason,
                        "tok_per_s": (tok_per_s * 10.0).round() / 10.0,
                    });
                    if s.drafted > 0 {
                        done["draft_tokens"] = serde_json::json!(s.drafted);
                        done["draft_accepted"] = serde_json::json!(s.accepted);
                    }
                    if s.think_forced {
                        done["think_forced"] = serde_json::json!(true);
                    }
                    let vt = timing_snapshot();
                    if !vt.is_empty() {
                        let mut m = serde_json::Map::new();
                        for (label, n, us) in vt {
                            m.insert(label, serde_json::json!({ "n": n, "us": us }));
                        }
                        done["verb_us"] = serde_json::Value::Object(m);
                    }
                    if let Some(e) = effort {
                        done["effort"] = serde_json::json!(e.as_str());
                        done["think_budget"] = serde_json::json!(params.think_budget);
                    }
                    if s.images > 0 {
                        done["images"] = serde_json::json!(s.images);
                        // what the pictures REALLY cost, from the host - the
                        // config's per-image budget is only for admission control
                        done["image_tokens"] = serde_json::json!(s.image_pos);
                    }
                    if let Some(t) = &tl {
                        if !t.log.is_empty() {
                            done["tools"] = serde_json::json!(t.log);
                        }
                    }
                    send(done);
                    ok = true;
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        break 'answer;
    }
    if !ok && !last_err.is_empty() {
        send(serde_json::json!({ "error": strip_code(&last_err) }));
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// --------------------------------------- OpenAI-compatible /v1 endpoints --

fn completion_id() -> String {
    format!("chatcmpl-enclave{:x}", now_ms())
}

fn handle_completions(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    // auth is deployment policy (top-level api_key), not per-model
    let base = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&base, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    // A client-declared `tools` array is the PASSTHROUGH (see client_tools):
    // the model is offered the client's functions and its call goes back on
    // the reply as `tool_calls`, for the CLIENT to execute. The registry the
    // deployment configured sits the turn out - the model sees ONE list.
    let client_reg = match creq.client_tools() {
        Ok(r) => r,
        Err(e) => return json_err(out, 400, &e),
    };
    let client_block = client_reg
        .as_ref()
        .map(|list| tools::client_system_block(list, creq.tool_must_call().as_deref()));
    let cfg = &match resolve_model(raw, creq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if let Err(e) = check_images(raw, cfg, &creq.messages) {
        return json_err(out, 400, &e);
    }
    let tok_bytes = match read_tokenizer(cfg) {
        Ok(b) => b,
        Err(e) => return json_err(out, 500, &e),
    };
    let tok = match Tokenizer::from_bytes(&tok_bytes) {
        Ok(t) => t,
        Err(e) => return json_err(out, 500, &format!("tokenizer: {e}")),
    };
    // `"web_search": true` is an Enclave extension to the OpenAI body, so API
    // clients get the same retrieval the built-in UI does. Sources come back
    // on the response's `enclave.search` field (streaming: an SSE comment
    // before the first chunk, since the chunk schema has nowhere to put them).
    let mode = creq.target.as_deref().unwrap_or("auto");
    let mut messages = creq.messages.clone();
    // OpenAI tool history (assistant `tool_calls`, role:"tool" results) into
    // the trained text forms, whether or not THIS request declares tools - an
    // agent's final wrap-up turn still carries the transcript
    fold_tool_history(&mut messages);
    let mut generated_image: Option<image::GeneratedImage> = None;
    let id = completion_id();
    let created = (now_ms() / 1000) as u64;
    let model = cfg.name.clone();

    if creq.stream.unwrap_or(false) {
        // ---- streaming: OpenAI chunk protocol over SSE.
        //
        // The response opens BEFORE the search/image/vision legs run. Those
        // legs can hold a turn for minutes (a diffusion job queued behind
        // other tenants, a big picture on a busy share), and response headers
        // held back that long are how proxy response timeouts and SDK read
        // timeouts kill the request before its first byte. Open first, and
        // the legs heartbeat as SSE comments, which conforming OpenAI parsers
        // are required to ignore. The trade: a leg failure now arrives as an
        // in-stream error event on a 200 rather than a 502 status - the same
        // trade /chat makes, with the same error text. Non-streaming keeps
        // its status codes below; it has no stream to keep warm.
        let headers = Fields::new();
        let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
        let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
        let resp = OutgoingResponse::new(headers);
        let body = resp.body().unwrap();
        ResponseOutparam::set(out, Ok(resp));
        let stream = body.write().unwrap();
        let send_raw = |s: &str| -> bool {
            for chunk in s.as_bytes().chunks(4000) {
                if stream.blocking_write_and_flush(chunk).is_err() {
                    return false;
                }
            }
            true
        };
        let chunk = |delta: serde_json::Value, finish: Option<&str>| -> String {
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": id, "object": "chat.completion.chunk", "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
                })
            )
        };
        // leg progress as SSE comments: the OpenAI protocol has no status
        // events, and a comment both narrates and keeps the connection warm
        let leg_status = |s: &str| {
            let _ = send_raw(&format!(": {s}\n\n"));
        };
        let send_err = |e: &str| {
            let _ = send_raw(&format!(
                "data: {}\n\n",
                serde_json::json!({ "error": { "message": strip_code(e), "type": "server_error" } })
            ));
        };
        // Effort scaling, when the deployment configured it: the rating comes out
        // of the router pass below if that runs, and from its own short pass later
        // if it does not. `want_effort` gates BOTH, so a deployment without the
        // block pays for neither.
        let mut router_effort: Option<Effort> = None;
        let want_effort = cfg.effort.is_some() && cfg.thinking && creq.thinking();
        let mut tl = if client_reg.is_some() {
            None
        } else {
            tools_enabled(cfg, &creq)
                .map(|tc| ToolLoop::open(tc, builtins_for(cfg, &creq), &leg_status))
        };
        if let Some(t) = &tl {
            for n in &t.reg.notes {
                let _ = send_raw(&format!(": enclave-tools {n}\n\n"));
            }
        }
        if tl.as_ref().is_some_and(|t| !t.armed()) {
            tl = None;
        }
        let model_searches = tl.as_ref().is_some_and(|t| t.owns_search());
        let search_meta = match apply_web_search(
            cfg,
            &creq,
            &mut messages,
            &tok,
            mode,
            &mut generated_image,
            &mut router_effort,
            want_effort,
            model_searches,
            &leg_status,
        ) {
            Ok(m) => m,
            Err(e) => {
                send_err(&e);
                drop(stream);
                let _ = OutgoingBody::finish(body, None);
                return;
            }
        };
        // the delegated vision leg (see apply_vision); a deployment with no
        // vision_service, or a serving model reading the picture itself, no-ops here
        let vision_meta = match apply_vision(cfg, &creq, &mut messages, &tok, mode, &leg_status) {
            Ok(m) => m,
            Err(e) => {
                send_err(&e);
                drop(stream);
                let _ = OutgoingBody::finish(body, None);
                return;
            }
        };
        // Sources first, as an SSE COMMENT: the chunk schema has no field for
        // them and inventing one would break strict OpenAI clients, while a
        // comment line is required to be ignored by every conforming parser.
        if let Some(m) = &search_meta {
            let _ = send_raw(&format!(": enclave-search {}\n\n", search_meta_json(m)));
        }
        // same trick for the delegated vision look: what was asked about the
        // picture and what it cost, where a strict OpenAI parser will ignore it
        if let Some(m) = &vision_meta {
            let _ = send_raw(&format!(": enclave-vision {}\n\n", vision_meta_json(m)));
        }
        // role preamble chunk (OpenAI clients expect it)
        let _ = send_raw(&chunk(serde_json::json!({ "role": "assistant" }), None));

        let mut last_err = String::new();
        let mut done_stats: Option<GenStats> = None;
        let mut client_calls: Vec<tools::ToolCall> = Vec::new();
        let (ref draft_cfg, _) = resolve_draft(raw, cfg);
        'answer: loop {
        let caps = match (&client_block, &tl) {
            (Some(b), _) => Capabilities::Client(b),
            (None, Some(t)) => Capabilities::Tools(t.tools(), t.cfg.max_calls),
            (None, None) => Capabilities::Note,
        };
        let (prompt_ids, stops, think_open) = match build_prompt(cfg, &tok, &messages, creq.thinking(), caps) {
            Ok(v) => v,
            Err(e) => {
                send_err(&e);
                drop(stream);
                let _ = OutgoingBody::finish(body, None);
                return;
            }
        };
        let effort =
            resolve_effort(cfg, &tok, &messages, mode, think_open, router_effort, &leg_status);
        let params = gen_params(cfg, &creq, stops, think_open, effort);
        for (target, tname) in targets_for(cfg, mode).iter() {
            // re-emit the prompt-side think opening ahead of the first real
            // delta (see handle_chat) so clients receive a complete block
            let opened = std::cell::Cell::new(!think_open);
            let gate = std::cell::RefCell::new(CallGate::new(
                tl.is_some() || client_block.is_some(),
                think_open,
            ));
            let emit = |delta: &str| {
                let Some(out) = gate.borrow_mut().push(delta) else { return true };
                if !opened.replace(true)
                    && !send_raw(&chunk(serde_json::json!({ "content": "<think>\n" }), None))
                {
                    return false;
                }
                send_raw(&chunk(serde_json::json!({ "content": out }), None))
            };
            // OpenAI protocol has no status events; SSE comments keep the
            // connection warm through cold session init without confusing SDKs
            let status = |s: &str| send_raw(&format!(": {s}\n\n"));
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &emit, &status) {
                Ok(s) => {
                    if let Some(t) = &mut tl {
                        // the call and its result travel as comments, for the
                        // same reason the sources do
                        let on_call = |c: &serde_json::Value| {
                            let _ = send_raw(&format!(": enclave-tool {c}\n\n"));
                        };
                        let on_result = |r: &serde_json::Value| {
                            let _ = send_raw(&format!(": enclave-tool-result {r}\n\n"));
                        };
                        if t.step(&s.text, &mut messages, &on_call, &on_result) {
                            continue 'answer;
                        }
                    }
                    if client_block.is_some() {
                        client_calls = tools::parse_calls(&s.text);
                        if !client_calls.is_empty() {
                            // the held call leaves as structured `tool_calls`
                            // below, not as content
                            gate.borrow_mut().drop_call();
                        }
                    }
                    if let Some(rest) = gate.borrow_mut().flush() {
                        if !opened.replace(true) {
                            let _ = send_raw(&chunk(serde_json::json!({ "content": "<think>\n" }), None));
                        }
                        let _ = send_raw(&chunk(serde_json::json!({ "content": rest }), None));
                    }
                    done_stats = Some(s);
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        break 'answer;
        }
        if let Some(t) = &tl {
            if !t.log.is_empty() {
                let _ = send_raw(&format!(": enclave-tools-ran {}\n\n", serde_json::json!(t.log)));
            }
        }
        match done_stats {
            Some(s) => {
                if client_calls.is_empty() {
                    let _ = send_raw(&chunk(serde_json::json!({}), Some(s.finish_reason)));
                } else {
                    // the whole call in one delta - the protocol allows
                    // argument fragments, but nothing here is gained by
                    // slicing what is already complete
                    let _ = send_raw(&chunk(
                        serde_json::json!({
                            "tool_calls": openai_tool_calls(&client_calls, now_ms(), true)
                        }),
                        None,
                    ));
                    let _ = send_raw(&chunk(serde_json::json!({}), Some("tool_calls")));
                }
                let _ = send_raw("data: [DONE]\n\n");
            }
            None => {
                let _ = send_raw(&format!(
                    "data: {}\n\n",
                    serde_json::json!({ "error": { "message": strip_code(&last_err), "type": "server_error" } })
                ));
            }
        }
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    } else {
        // ---- non-streaming: run to completion, one JSON response. The legs
        // run before the response by necessity - there is no stream to
        // heartbeat - so this path keeps real HTTP status codes and keeps the
        // standing advice: a request that can take minutes (image generation,
        // a big vision read) belongs on stream:true, because the buffered
        // path has a proxy-hop timeout budget it cannot influence.
        let no_status = |_: &str| {};
        // Effort scaling, when the deployment configured it: the rating comes out
        // of the router pass below if that runs, and from its own short pass later
        // if it does not. `want_effort` gates BOTH, so a deployment without the
        // block pays for neither.
        let mut router_effort: Option<Effort> = None;
        let want_effort = cfg.effort.is_some() && cfg.thinking && creq.thinking();
        let mut tl = if client_reg.is_some() {
            None
        } else {
            tools_enabled(cfg, &creq)
                .map(|tc| ToolLoop::open(tc, builtins_for(cfg, &creq), &no_status))
        };
        if tl.as_ref().is_some_and(|t| !t.armed()) {
            tl = None;
        }
        let model_searches = tl.as_ref().is_some_and(|t| t.owns_search());
        let search_meta = match apply_web_search(
            cfg,
            &creq,
            &mut messages,
            &tok,
            mode,
            &mut generated_image,
            &mut router_effort,
            want_effort,
            model_searches,
            &no_status,
        ) {
            Ok(m) => m,
            Err(e) => return json_err(out, 502, &e),
        };
        let vision_meta = match apply_vision(cfg, &creq, &mut messages, &tok, mode, &no_status) {
            Ok(m) => m,
            Err(e) => return json_err(out, 502, &e),
        };
        let sink = |_: &str| true;
        let mut last_err = String::new();
        let mut result: Option<GenStats> = None;
        let (mut think_open, mut effort, mut params);
        let (ref draft_cfg, _) = resolve_draft(raw, cfg);
        'answer: loop {
        let caps = match (&client_block, &tl) {
            (Some(b), _) => Capabilities::Client(b),
            (None, Some(t)) => Capabilities::Tools(t.tools(), t.cfg.max_calls),
            (None, None) => Capabilities::Note,
        };
        let (prompt_ids, stops, opened) = match build_prompt(cfg, &tok, &messages, creq.thinking(), caps) {
            Ok(v) => v,
            Err(e) => return json_err(out, 400, &e),
        };
        think_open = opened;
        effort = resolve_effort(cfg, &tok, &messages, mode, think_open, router_effort, &no_status);
        params = gen_params(cfg, &creq, stops, think_open, effort);
        for (target, tname) in targets_for(cfg, mode).iter() {
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &sink, &sink) {
                Ok(s) => {
                    if let Some(t) = &mut tl {
                        let quiet = |_: &serde_json::Value| {};
                        if t.step(&s.text, &mut messages, &quiet, &quiet) {
                            continue 'answer;
                        }
                    }
                    result = Some(s);
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        break 'answer;
        }
        match result {
            Some(s) => {
                let client_calls = if client_block.is_some() {
                    tools::parse_calls(&s.text)
                } else {
                    Vec::new()
                };
                let (message, finish) = if client_calls.is_empty() {
                    // the prompt force-opened the think block; restore the tag
                    // so the reply carries a complete one
                    let content =
                        if think_open { format!("<think>\n{}", s.text) } else { s.text.clone() };
                    (
                        serde_json::json!({ "role": "assistant", "content": content }),
                        s.finish_reason,
                    )
                } else {
                    // the reasoning stays as content, the call leaves as
                    // structured `tool_calls`; null content when there was
                    // nothing but the call, which is what SDKs expect
                    let visible = visible_before_call(&s.text);
                    let content = if visible.is_empty() {
                        serde_json::Value::Null
                    } else if think_open {
                        serde_json::json!(format!("<think>\n{visible}"))
                    } else {
                        serde_json::json!(visible)
                    };
                    (
                        serde_json::json!({
                            "role": "assistant", "content": content,
                            "tool_calls": openai_tool_calls(&client_calls, now_ms(), false),
                        }),
                        "tool_calls",
                    )
                };
                let mut body_json = serde_json::json!({
                    "id": id, "object": "chat.completion", "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": finish,
                    }],
                    "usage": {
                        "prompt_tokens": s.prompt_tokens,
                        "completion_tokens": s.tokens,
                        "total_tokens": s.prompt_tokens + s.tokens,
                    },
                    "enclave": { "target": s.target, "load_ms": s.load_ms as u64,
                             "prefill_ms": s.prefill_ms as u64, "decode_ms": s.decode_ms as u64,
                             "draft_tokens": s.drafted, "draft_accepted": s.accepted,
                             "think_forced": s.think_forced,
                             "effort": effort.map(|e| e.as_str()),
                             "think_budget": effort.map(|_| params.think_budget) },
                });
                if s.images > 0 {
                    // usage.prompt_tokens counts the text; images are priced
                    // in positions and reported beside it rather than folded
                    // in, so an OpenAI client's arithmetic still adds up
                    body_json["enclave"]["images"] = serde_json::json!(s.images);
                    body_json["enclave"]["image_tokens"] = serde_json::json!(s.image_pos);
                }
                if let Some(m) = &search_meta {
                    body_json["enclave"]["search"] = search_meta_json(m);
                }
                if let Some(m) = &vision_meta {
                    body_json["enclave"]["vision"] = vision_meta_json(m);
                }
                if let Some(t) = &tl {
                    if !t.log.is_empty() {
                        body_json["enclave"]["tools"] = serde_json::json!(t.log);
                    }
                }
                respond_bytes(out, 200, "application/json", body_json.to_string().as_bytes());
            }
            None => json_err(out, 500, &last_err),
        }
    }
}

// ------------------------------------------------------------------ warmup --

/// Warm one model: open a session and feed a single in-vocab token, which
/// forces the full compute path (workspace allocation, kernel warm) without
/// generating anything a user could see. Returns (target, load_ms, feed_ms).
/// Repeat calls are cheap (the load coalesces on the host's session cache /
/// preloaded graph). An error here IS the fit signal the ladder consumes:
/// an absent host graph, a failed context/KV allocation and a failed first
/// compute all mean this model does not serve under the current share.
fn warm_one(cfg: &AppConfig, mode: &str) -> Result<(String, u64, u64), String> {
    let warm_tok = cfg.eos.first().copied().unwrap_or(0);
    let mut last_err = String::new();
    for (target, tname) in targets_for(cfg, mode) {
        let t0 = now_ms();
        let opened = Session::open(cfg, target);
        let load_ms = now_ms() - t0;
        let t1 = now_ms();
        match opened.and_then(|mut sess| sess.feed(cfg, &[warm_tok], true)) {
            Ok(_) => return Ok((tname.to_string(), load_ms as u64, (now_ms() - t1) as u64)),
            Err(e) => last_err = format!("{tname}: {e}"),
        }
    }
    Err(last_err)
}

/// GET /warmup - put weights and kernels in device memory BEFORE the first
/// real prompt.
///
/// `?model=<name|volume>` warms that ONE model (the playground re-warms on
/// selection change); response shape and error semantics are the classic
/// single-model ones. BARE `/warmup` is the LADDER: every servable model,
/// SMALLEST FIRST, warmed one at a time, failures recorded and skipped.
/// Smallest-first is deliberate on both axes: residency within the share is
/// first-come-first-served, so the models most likely to fit are resident
/// (and guaranteed) before a bigger sibling claims - or fails to claim -
/// the rest; and one published app degrades gracefully across deployment
/// sizes: a small share serves the small models and reports the big ones
/// unfit, a bigger share unlocks them. The manager's boot warmup GETs the
/// bare path, so a fresh deployment sorts itself out at launch; the
/// playground fires it on page load and disables the unfit models in its
/// menu. 200 with per-model results while at least one model warmed, 500
/// when none did.
///
/// ?target= defaults to GPU ONLY - warmup exists to put weights in VRAM,
/// and a failed GPU should read as a failed warmup, not silently pre-build
/// the CPU session (chat's auto mode still falls back at request time). The
/// one exception is a deployment with no GPU share at all (gpu_present),
/// where the default drops to auto: there is no VRAM to fail into, and
/// "every model broken" would bury the actual story the playground tells.
/// Pass target=cpu (dev boxes) or target=auto explicitly to warm other
/// paths. Slow by design when cold - the response arrives when the models
/// are ready.
fn handle_warmup(raw: &serde_json::Value, query: &str, out: ResponseOutparam) {
    // GPU by default - but on a deployment the platform gave NO GPU share,
    // "gpu" is a guaranteed failure for the onnx path, and reporting every
    // model broken hides the real story (the fleet had no GPU enclave free).
    // Degrade to auto there, so the CPU session warms and the playground's
    // CPU-mode notice is what the user sees.
    let mode = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("target="))
        .unwrap_or(if gpu_present() == Some(false) { "auto" } else { "gpu" });
    let model = query.split('&').find_map(|kv| kv.strip_prefix("model="));

    if model.is_some() {
        // single-model mode; unknown names fall back to the default model
        // (resolve_model semantics), no servable models to the top-level
        // config whose volume-not-attached error says what to attach
        let cfg = &match resolve_model(raw, model) {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        return match warm_one(cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            // an ACTIVE session holding the slot is proof the model serves -
            // report warm, not failed (a 500 here would make the playground
            // call a busy model unfit and disable it in the picker)
            Err(e) if e.contains(BUSY_MARKER) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "busy": true,
                    "note": "another chat holds the inference session right now; the model is loaded and serving",
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            Err(e) => json_err(out, 500, &e),
        };
    }

    // ladder mode: smallest first (available_models sorts largest-first)
    let mut entries = available_models(raw);
    entries.reverse();
    if entries.is_empty() {
        let cfg = &match resolve_model(raw, None) {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        return match warm_one(cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            Err(e) => json_err(out, 500, &e),
        };
    }
    // models the VRAM budget certainly cannot hold are reported unfit
    // WITHOUT probing - no point starting a multi-GB load that must OOM.
    // CPU-target warms skip the gate (dev boxes have no VRAM budget).
    let unfit = if mode == "cpu" {
        std::collections::HashMap::new()
    } else {
        over_budget(&entries)
    };
    let mut ladder = Vec::with_capacity(entries.len());
    let mut default: Option<String> = None; // largest warmed = last ok in ascending order
    for e in &entries {
        if let Some(why) = unfit.get(&e.volume) {
            ladder.push(serde_json::json!({
                "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                "ok": false, "skipped": true, "error": why,
            }));
            continue;
        }
        match warm_one(&e.cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                default = Some(e.cfg.name.clone());
                ladder.push(serde_json::json!({
                    "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                    "ok": true, "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                }));
            }
            // busy = an active chat holds the session slot; the model is
            // loaded and serving, which is exactly what "warm" means here
            Err(err) if err.contains(BUSY_MARKER) => {
                default = Some(e.cfg.name.clone());
                ladder.push(serde_json::json!({
                    "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                    "ok": true, "busy": true,
                }));
            }
            Err(err) => ladder.push(serde_json::json!({
                "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                "ok": false, "error": strip_code(&err),
            })),
        }
    }
    let ok = default.is_some();
    let body = serde_json::json!({ "ok": ok, "ladder": ladder, "default": default });
    respond_bytes(
        out,
        if ok { 200 } else { 500 },
        "application/json",
        body.to_string().as_bytes(),
    );
}

/// GET /models - the playground's dropdown source: servable models largest
/// (= default) first. Open like the playground itself; volume names are
/// already surfaced by error messages, and weights sizes are public catalog
/// facts, so nothing here needs the API key.
fn handle_model_list(raw: &serde_json::Value, out: ResponseOutparam) {
    let entries = available_models(raw);
    let unfit = over_budget(&entries);
    // ENCLAVE_NN_PRELOADS: which ggml volumes the host actually loaded at
    // boot. `preloaded: false` on a fitting model lets the playground say
    // "waiting for the host" up front instead of probing into a NotFound
    // and calling the model missing. Absent on older managers (no field).
    let preloads = preloaded_graphs();
    let body = serde_json::json!({
        "default": entries.first().map(|e| e.cfg.name.clone()),
        "vram_budget": vram_budget(),
        // true / false / null (unknown) - the playground raises its CPU-mode
        // notice on an explicit false, never on a null
        "gpu": gpu_present(),
        "models": entries.iter().enumerate().map(|(i, e)| {
            let mut m = serde_json::json!({
                "name": e.cfg.name, "volume": e.volume, "backend": e.cfg.backend,
                "bytes": e.bytes, "default": i == 0,
                "fits": !unfit.contains_key(&e.volume),
                "thinking": e.cfg.thinking,
                // whether this model can READ pictures. The playground offers
                // the attach button per model, so switching to a text-only
                // model visibly takes the capability away instead of failing
                // a turn the user has already typed.
                "vision": e.cfg.vision && e.cfg.backend == "ggml",
            });
            if let Some(why) = unfit.get(&e.volume) {
                m["why"] = serde_json::json!(why);
            }
            if e.cfg.backend == "ggml" {
                if let Some(pre) = &preloads {
                    m["preloaded"] = serde_json::json!(pre.iter().any(|p| p == &e.volume));
                }
            }
            m
        }).collect::<Vec<_>>(),
    });
    let mut body = body;
    // the playground hides its web-search control unless the deployment
    // configured a provider - an always-visible button that 502s is worse
    // than no button. Provider name only; the key is never exposed.
    let base_cfg = config::from_value(raw.clone()).ok();
    body["search"] = match base_cfg.as_ref().and_then(|c| c.search.clone()) {
        Some(s) => serde_json::json!({ "enabled": true, "provider": s.provider,
                                       "fetch_pages": s.fetch_pages,
                                       "default_on": s.default_on }),
        None => serde_json::json!({ "enabled": false }),
    };
    // the endpoint is NOT exposed: it is deployment topology, and the browser
    // never talks to it - only this app does
    body["image"] = match base_cfg.as_ref().and_then(|c| c.image.clone()) {
        Some(i) => serde_json::json!({ "enabled": true, "size": i.size, "model": i.model,
                                       "default_on": i.default_on }),
        None => serde_json::json!({ "enabled": false }),
    };
    // Vision is normally a MODEL property (each entry carries its own flag
    // above), EXCEPT when the deployment configured a vision service: that
    // reads for every model, so the composer must offer the attach button
    // whatever is selected. `service` is the flag it uses for that; the
    // endpoint is NOT exposed - it is deployment topology, and the browser
    // never talks to it, only this app does.
    // Tools: the playground shows a switch when the deployment configured any.
    // NAMES only - a URL is deployment topology, a header may carry a
    // credential, and the browser never talks to either; only this app does.
    // `default_on` tells the switch where to start; it still starts off unless
    // the deployment deliberately said otherwise.
    body["tools"] = match base_cfg.as_ref().and_then(|c| c.tools.clone()) {
        Some(t) if !t.is_empty() => {
            serde_json::json!({
                "enabled": true,
                "default_on": t.default_on,
                "max_calls": t.max_calls,
                // what a turn would really be offered, not what was typed: a
                // duplicate or an unusable name is dropped at resolution, and
                // naming it here would put a tool in the UI nothing can call
                "http": t.http_names(),
                // an MCP server's tools are only known after discovery, which
                // happens per turn - so the count is what can be promised here
                "mcp": t.mcp.len(),
            })
        }
        _ => serde_json::json!({ "enabled": false }),
    };
    let vision_any = entries.iter().any(|e| e.cfg.vision && e.cfg.backend == "ggml");
    let service = base_cfg.as_ref().map(|c| c.vision_service.is_some()).unwrap_or(false);
    body["vision"] = serde_json::json!({
        "enabled": vision_any || service,
        "service": service,
        "max_images": base_cfg.as_ref().map(|c| c.max_images).unwrap_or(0),
        "max_bytes": base_cfg.as_ref().map(|c| c.max_image_bytes).unwrap_or(0),
    });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

/// GET /search?q=... - the egress probe. Runs exactly the path a chat turn
/// would (same provider, same timeouts, same page fetches) and returns the
/// hits as JSON, so "does this deployment actually reach the internet" is one
/// curl and not a chat transcript to squint at.
///
/// Behind the SAME api_key gate as /v1, unlike the other playground routes.
/// `?url=` fetches an arbitrary URL from this deployment's egress identity and
/// hands back the text: left open, that is a general-purpose relay anyone can
/// point anywhere and have the enclave's address wear it. A deployment with no
/// api_key is open by policy (gate it by deploying PRIVATE), but where the
/// operator did set one, this must not be the hole in it.
/// This deployment's own hardware attestation, for the dialog behind the shield
/// icon. OPEN, like /models: what the hardware is has to be readable BEFORE you
/// decide to type into the box, so putting it behind the deployment's API key
/// would defeat the purpose.
///
/// Failure is normal and is reported as such: a deployment with no egress leg
/// cannot reach the platform API at all, and the dialog then falls back to the
/// deployment id and the verify-it-yourself links.
fn handle_attestation(req: IncomingRequest, out: ResponseOutparam) {
    let host = req.authority();
    match attest::document(host.as_deref()) {
        Ok(doc) => respond_public_json(out, 200, doc.to_string().as_bytes()),
        // 503, not 500: nothing is broken in the app - the attestation is
        // momentarily unreachable from in here, and the page says so. Same
        // open-CORS shape as success, so a cross-origin verifier reads the
        // real reason instead of an opaque network error.
        Err(e) => respond_public_json(
            out,
            503,
            serde_json::json!({ "error": { "message": e, "type": "invalid_request_error" } })
                .to_string()
                .as_bytes(),
        ),
    }
}

/// Per-deployment /.well-known files - Android's assetlinks.json, Apple's
/// apple-app-site-association - for the mobile shells wrapping this app. The
/// VALUES ride the deployment config (`well_known` object, filename -> JSON),
/// because they carry per-customer signing-cert fingerprints and team ids
/// that must never require republishing the wasm.
fn well_known_lookup(raw: &serde_json::Value, name: &str) -> Option<String> {
    raw.get("well_known")?
        .as_object()?
        .get(name)
        .map(|v| v.to_string())
}

fn handle_search_probe(
    raw: &serde_json::Value,
    req: IncomingRequest,
    query: &str,
    out: ResponseOutparam,
) {
    let cfg = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&cfg, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let Some(scfg) = &cfg.search else {
        return json_err(out, 501, "web search is not enabled on this deployment");
    };
    // ?url=<page> fetches and extracts ONE page, skipping the provider. It
    // separates the two things that can be broken - "can this deployment
    // reach the web at all" from "is the search provider talking to us" -
    // which otherwise fail identically from the outside.
    if let Some(u) = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("url="))
        .map(percent_decode_query)
    {
        return match search::fetch_page(scfg, &u) {
            Ok(text) => {
                let body = serde_json::json!({
                    "url": u,
                    "chars": text.chars().count(),
                    "preview": text.chars().take(600).collect::<String>(),
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            Err(e) => json_err(out, 502, &e),
        };
    }
    let q = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("q="))
        .map(percent_decode_query)
        .unwrap_or_default();
    if q.trim().is_empty() {
        return json_err(out, 400, "usage: GET /search?q=<query> | GET /search?url=<page>");
    }
    let t0 = now_ms();
    match search::search(scfg, &q) {
        Ok(hits) => {
            let body = serde_json::json!({
                "query": q,
                "provider": scfg.provider,
                "ms": now_ms().saturating_sub(t0),
                "hits": hits.iter().map(|h| serde_json::json!({
                    "title": h.title, "url": h.url, "snippet": h.snippet,
                    "body_chars": h.body.as_ref().map(|b| b.chars().count()),
                })).collect::<Vec<_>>(),
            });
            respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
        }
        Err(e) => json_err(out, 502, &e),
    }
}

/// GET /tools - the tool-leg probe, and the exact counterpart of /search?q=.
///
/// Resolving the registry is where a tools deployment goes wrong: an MCP server
/// that will not answer, a `$SECRET` nobody set, a name collision that silently
/// dropped an entry. All of those look identical from a chat window - the model
/// simply never calls the thing - so this runs the same resolution a turn runs
/// and shows what came back, with no inference in the way.
///
/// `?call=<name>&args=<json>` then executes ONE tool, which separates "can this
/// deployment see the tool" from "does the tool work". Behind the API key,
/// because it reaches an endpoint from this deployment's egress identity.
fn handle_tools_probe(
    raw: &serde_json::Value,
    req: IncomingRequest,
    query: &str,
    out: ResponseOutparam,
) {
    let cfg = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&cfg, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let Some(tcfg) = &cfg.tools else {
        return json_err(out, 501, "no tools are configured on this deployment");
    };
    let param = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .map(percent_decode_query)
    };
    let t0 = now_ms();
    let b = builtins_of(&cfg);
    let mut reg = tools::build(tcfg, b, &|_| {});
    let discover_ms = now_ms().saturating_sub(t0);
    if let Some(name) = param("call") {
        let args: serde_json::Value = match param("args") {
            Some(a) => match serde_json::from_str(&a) {
                Ok(v) => v,
                Err(e) => return json_err(out, 400, &format!("args is not valid JSON: {e}")),
            },
            None => serde_json::json!({}),
        };
        let r = tools::call(&mut reg, tcfg, b, &name, &args, || now_ms() as u64);
        let mut body = serde_json::json!({
            "name": name, "arguments": args, "ok": !r.is_error, "ms": r.ms,
            "result": r.text,
        });
        if !r.sources.is_empty() {
            body["sources"] = serde_json::json!(r.sources);
        }
        return respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
    }
    let body = serde_json::json!({
        "discover_ms": discover_ms,
        "max_calls": tcfg.max_calls,
        "default_on": tcfg.default_on,
        "notes": reg.notes,
        "tools": reg.tools.iter().map(|t| serde_json::json!({
            "name": t.name,
            "description": t.description,
            "source": match &t.src {
                tools::ToolSrc::Builtin(_) => "builtin".to_string(),
                tools::ToolSrc::Http(_) => "http".to_string(),
                tools::ToolSrc::Mcp { server, remote } =>
                    format!("mcp[{server}]:{remote}"),
                // the probe resolves the deployment's registry, which never
                // holds a client-declared tool
                tools::ToolSrc::Client => "client".to_string(),
            },
            "parameters": t.parameters,
        })).collect::<Vec<_>>(),
    });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

/// `+`-and-`%XX` decode for one query-string value.
fn percent_decode_query(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle_models(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let base = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&base, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let entries = available_models(raw);
    let data: Vec<serde_json::Value> = if entries.is_empty() {
        // nothing servable attached: advertise the configured name so SDK
        // flows still see a model id (requests will explain what to attach)
        vec![serde_json::json!({ "id": base.name, "object": "model", "owned_by": "enclave-deployment" })]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                serde_json::json!({
                    "id": e.cfg.name, "object": "model", "owned_by": "enclave-deployment",
                    "enclave": { "volume": e.volume, "backend": e.cfg.backend,
                             "bytes": e.bytes, "default": i == 0 },
                })
            })
            .collect()
    };
    let body = serde_json::json!({ "object": "list", "data": data });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

/// Does this path address an open conversation? Exactly `/c`, `/c/`, or
/// `/c/<id>` with an optional trailing slash. Deeper paths are REFUSED rather
/// than served the page: the page pins its <base> by stripping one `/c/<id>`
/// segment, so `/c/a/b` would load a playground whose every fetch resolves one
/// directory too deep and 404s. A wrong URL should fail as a wrong URL.
fn is_chat_path(p: &str) -> bool {
    if p == "/c" || p == "/c/" {
        return true;
    }
    match p.strip_prefix("/c/") {
        Some(rest) => {
            let id = rest.trim_end_matches('/');
            !id.is_empty() && !id.contains('/')
        }
        None => false,
    }
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let raw = match config::load_raw() {
            Ok(v) => v,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/");
        let query = pq.split_once('?').map(|(_, q)| q).unwrap_or("");
        let method = req.method();
        match (method, path) {
            (Method::Get, "/") | (Method::Get, "") => {
                respond_bytes(out, 200, "text/html; charset=utf-8", CHAT_HTML.as_bytes())
            }
            // The open conversation is addressed by PATH (/c/<id>), so every one
            // of those paths serves the same page and the client reads the id
            // out of its own pathname. Nothing here resolves the id: it is an
            // IndexedDB key in ONE browser profile, so this deployment has never
            // heard of it and could not honour it if it wanted to.
            (Method::Get, p) if is_chat_path(p) => {
                respond_bytes(out, 200, "text/html; charset=utf-8", CHAT_HTML.as_bytes())
            }
            (Method::Get, "/emoji.woff2") => respond_asset(out, "font/woff2", EMOJI_WOFF2),
            (Method::Get, "/favicon.svg") | (Method::Get, "/icon.svg") => {
                respond_asset(out, "image/svg+xml", FAVICON_SVG.as_bytes())
            }
            (Method::Get, "/favicon.ico") => respond_asset(out, "image/x-icon", FAVICON_ICO),
            (Method::Get, "/apple-touch-icon.png")
            | (Method::Get, "/apple-touch-icon-precomposed.png") => {
                respond_asset(out, "image/png", TOUCH_ICON_PNG)
            }
            (Method::Get, "/icon-192.png") => respond_asset(out, "image/png", ICON_192_PNG),
            (Method::Get, "/icon-512.png") => respond_asset(out, "image/png", ICON_512_PNG),
            (Method::Get, "/icon-maskable-512.png") => {
                respond_asset(out, "image/png", ICON_MASKABLE_PNG)
            }
            // no-cache rather than immutable: at a stable custom domain these
            // two are how a NEW version announces itself, so they must never
            // outlive the version that served them
            (Method::Get, "/manifest.webmanifest") => respond_with_cache(
                out,
                200,
                "application/manifest+json",
                MANIFEST_JSON,
                Some("no-cache"),
            ),
            (Method::Get, "/sw.js") => respond_with_cache(
                out,
                200,
                "text/javascript; charset=utf-8",
                SW_JS.replace("__REV__", ASSET_REV).as_bytes(),
                Some("no-cache"),
            ),
            (Method::Get, p) if p.starts_with("/.well-known/") => {
                match well_known_lookup(&raw, &p["/.well-known/".len()..]) {
                    Some(body) => respond_public_json(out, 200, body.as_bytes()),
                    None => json_err(
                        out,
                        404,
                        "no such well-known file; this deployment's config declares none by that name under \"well_known\"",
                    ),
                }
            }
            (Method::Get, "/ping") => respond_bytes(
                out,
                200,
                "application/json",
                format!("{{\"ok\":true,\"pong\":true,\"t\":{}}}", now_ms()).as_bytes(),
            ),
            (Method::Get, "/models") => handle_model_list(&raw, out),
            (Method::Get, "/attestation") => handle_attestation(req, out),
            (Method::Get, "/search") => handle_search_probe(&raw, req, query, out),
            (Method::Get, "/tools") => handle_tools_probe(&raw, req, query, out),
            (Method::Get, "/warmup") => handle_warmup(&raw, query, out),
            (Method::Post, "/chat") => handle_chat(&raw, req, out),
            (Method::Post, "/title") => handle_title(&raw, req, out),
            (Method::Post, "/v1/chat/completions") => handle_completions(&raw, req, out),
            (Method::Get, "/v1/models") => handle_models(&raw, req, out),
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /c/<chat>, GET /favicon.svg, GET /favicon.ico, GET /apple-touch-icon.png, GET /icon-192.png, GET /icon-512.png, GET /icon-maskable-512.png, GET /manifest.webmanifest, GET /sw.js, GET /.well-known/<file>, GET /emoji.woff2, GET /ping, GET /models, GET /attestation, GET /search, GET /warmup, GET /v1/models, POST /v1/chat/completions, POST /chat, POST /title",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    /// Routes the router serves statics from, spelled scope-relative the way
    /// the manifest and the worker's precache list must spell them (the app
    /// also serves under the /x/<id>/https/ prefix, where an absolute path
    /// would escape the app).
    const SHELL_ROUTES: &[&str] = &[
        "",
        "emoji.woff2",
        "favicon.svg",
        "favicon.ico",
        "apple-touch-icon.png",
        "manifest.webmanifest",
        "icon-192.png",
        "icon-512.png",
        "icon-maskable-512.png",
    ];

    #[test]
    fn manifest_parses_and_stays_base_relative() {
        let m: serde_json::Value =
            serde_json::from_slice(MANIFEST_JSON).expect("manifest is valid JSON");
        for key in ["start_url", "scope", "id"] {
            let v = m[key].as_str().unwrap_or_else(|| panic!("manifest has {key}"));
            assert!(
                !v.starts_with('/'),
                "manifest {key} must stay base-relative, got {v}"
            );
        }
        let icons = m["icons"].as_array().expect("manifest has icons");
        assert!(!icons.is_empty());
        for icon in icons {
            let src = icon["src"].as_str().expect("icon has src");
            assert!(
                SHELL_ROUTES.contains(&src),
                "manifest icon {src} is not a route the router serves"
            );
        }
    }

    #[test]
    fn sw_precache_list_matches_served_routes() {
        let list = SW_JS
            .split("var SHELL = [")
            .nth(1)
            .and_then(|rest| rest.split("];").next())
            .expect("sw.js declares var SHELL = [...]");
        let entries: Vec<&str> = list.split('"').skip(1).step_by(2).collect();
        assert!(!entries.is_empty());
        for e in &entries {
            assert!(
                SHELL_ROUTES.contains(e),
                "sw.js precaches {e:?}, which the router does not serve"
            );
        }
        assert!(
            entries.contains(&""),
            "the page itself must be in the precache or offline serves nothing"
        );
    }

    #[test]
    fn well_known_serves_only_declared_files() {
        let raw = serde_json::json!({
            "well_known": {
                "assetlinks.json": [{ "relation": ["delegate_permission/common.handle_all_urls"] }],
                "apple-app-site-association": { "applinks": { "details": [] } }
            }
        });
        let body = well_known_lookup(&raw, "assetlinks.json").expect("declared file serves");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("served body is JSON");
        assert!(parsed.is_array());
        assert!(well_known_lookup(&raw, "apple-app-site-association").is_some());
        assert!(well_known_lookup(&raw, "security.txt").is_none(), "undeclared name is a 404");
        assert!(well_known_lookup(&raw, "").is_none());
        assert!(well_known_lookup(&serde_json::json!({}), "assetlinks.json").is_none(), "no well_known key at all");
        assert!(
            well_known_lookup(&serde_json::json!({ "well_known": 7 }), "assetlinks.json").is_none(),
            "a non-object well_known serves nothing rather than panicking"
        );
    }

    #[test]
    fn sw_rev_is_stamped() {
        assert_eq!(ASSET_REV.len(), 16, "build.rs emits a 16-hex-char rev");
        assert!(ASSET_REV.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            SW_JS.contains("\"__REV__\""),
            "sw.js must carry the placeholder the /sw.js route stamps"
        );
        assert!(!SW_JS.replace("__REV__", ASSET_REV).contains("__REV__"));
    }

    fn search_of(t: &str) -> Option<String> {
        match parse_router_verdict(t, true, true) {
            Some(RouterVerdict::Search(q)) => Some(q),
            _ => None,
        }
    }
    fn image_of(t: &str) -> Option<String> {
        match parse_router_verdict(t, true, true) {
            Some(RouterVerdict::Image(p)) => Some(p),
            _ => None,
        }
    }

    /// A tiny real tokenizer, so the prompt-splitting tests exercise the same
    /// code path production does instead of a stand-in. Word level, unknown
    /// words map to [UNK]: the tests care about STRUCTURE (which runs of text
    /// became which parts), never about the specific ids.
    fn test_tokenizer() -> Tokenizer {
        let json = r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],
            "normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,
            "decoder":null,
            "model":{"type":"WordLevel","vocab":{"[UNK]":0,"before":1,"after":2,"caption":3,
            "this":4,"no":5,"pictures":6,"here":7,"slot":8},"unk_token":"[UNK]"}}"#;
        Tokenizer::from_bytes(json.as_bytes()).expect("test tokenizer")
    }

    /// The app's own embedded defaults, which is what a deployment starts from.
    fn test_config() -> AppConfig {
        let v: serde_json::Value =
            serde_json::from_slice(config::APP_CONFIG_JSON).expect("embedded config is JSON");
        config::from_value(v).expect("embedded config parses")
    }

    fn chat_req(body: serde_json::Value) -> ChatReq {
        serde_json::from_value(body).expect("request parses")
    }

    /// The gate's whole job: a call never reaches the client, an answer is not
    /// delayed by more than the few characters it takes to tell them apart.
    #[test]
    fn a_tool_call_is_never_streamed_to_the_client() {
        let mut g = CallGate::new(true, false);
        let mut seen = String::new();
        for d in ["<tool", "_call>", "\n{\"name\": \"a\"", ", \"arguments\": {}}"] {
            if let Some(s) = g.push(d) {
                seen.push_str(&s);
            }
        }
        assert_eq!(seen, "", "the call leaked to the client");
        // and it was NOT lost: nothing executed it, so it is still deliverable
        assert!(g.flush().unwrap().contains("tool_call"));
    }

    #[test]
    fn an_ordinary_answer_flows_almost_immediately() {
        let mut g = CallGate::new(true, false);
        // "The" diverges from every opener on the first character
        assert_eq!(g.push("The").as_deref(), Some("The"));
        assert_eq!(g.push(" answer").as_deref(), Some(" answer"));
        assert_eq!(g.flush(), None);
    }

    /// Reasoning is not held back: a model that thinks for thirty seconds
    /// still streams for thirty seconds, and only the body is judged.
    #[test]
    fn reasoning_streams_while_the_body_waits() {
        let mut g = CallGate::new(true, true);
        assert_eq!(g.push("I should look this up.").as_deref(), Some("I should look this up."));
        // the tag flows; the newline after it is the start of the body, so it
        // waits with everything else until the body can be judged
        assert_eq!(g.push("\n</think>\n").as_deref(), Some("\n</think>"));
        assert_eq!(g.push("<tool_call>"), None);
        assert_eq!(g.push("{\"name\":\"a\"}"), None);
        // the reasoning went out, the call did not
        assert!(g.flush().unwrap().trim_start().starts_with("<tool_call>"));
    }

    #[test]
    fn an_unarmed_gate_is_a_pipe() {
        let mut g = CallGate::new(false, false);
        assert_eq!(g.push("<tool_call>").as_deref(), Some("<tool_call>"));
        assert_eq!(g.flush(), None);
    }

    /// Who gets tools: the deployment has to configure them, the model has to
    /// be one the format was trained into, and the request has to want them.
    #[test]
    fn tools_are_off_until_everything_agrees() {
        let mut cfg = test_config();
        cfg.template = "chatml".into();
        let on = serde_json::json!({ "messages": [], "tools": true });
        // no config block at all
        assert!(tools_enabled(&cfg, &chat_req(on.clone())).is_none());

        cfg.tools = Some(
            serde_json::from_value(serde_json::json!({
                "http": [{ "name": "t", "url": "https://h/x" }]
            }))
            .unwrap(),
        );
        // configured and asked for
        assert!(tools_enabled(&cfg, &chat_req(on.clone())).is_some());
        // configured but the request said nothing, and default_on is false:
        // reaching outside is never a default someone inherits
        assert!(tools_enabled(&cfg, &chat_req(serde_json::json!({ "messages": [] }))).is_none());
        // ...unless the deployment says so
        cfg.tools.as_mut().unwrap().default_on = true;
        assert!(tools_enabled(&cfg, &chat_req(serde_json::json!({ "messages": [] }))).is_some());
        // an OpenAI client turns them off the way it knows how
        let off = serde_json::json!({ "messages": [], "tool_choice": "none" });
        assert!(tools_enabled(&cfg, &chat_req(off)).is_none());
        // a template the format was never trained into gets nothing
        cfg.template = "llama3".into();
        assert!(tools_enabled(&cfg, &chat_req(on)).is_none());
    }

    /// A search the user asked for that could not be run must not kill the
    /// turn: the model is told, in the turn, and instructed to say the answer
    /// is unverified. (The retrieval failure itself degrades in finish_search;
    /// this pins the note that makes the degradation honest.)
    #[test]
    fn a_failed_search_leaves_the_turn_alive_with_a_note() {
        let mut msgs =
            vec![ChatMsg::text("system", "be brief"), ChatMsg::text("user", "what changed today?")];
        note_failed_search(&mut msgs, 1);
        assert!(msgs[1].content.starts_with("what changed today?"), "{}", msgs[1].content);
        assert!(msgs[1].content.contains("FAILED"), "{}", msgs[1].content);
        assert!(msgs[1].content.contains("unverified"), "{}", msgs[1].content);
        // the note rides the user turn, never the system prompt
        assert_eq!(msgs[0].content, "be brief");
    }

    /// The two switches are independent, and absent means the deployment's
    /// default: images on, search off. A user who will not send questions to a
    /// search provider keeps the picture generator the operator runs.
    #[test]
    fn search_and_images_switch_separately() {
        let req = |v: serde_json::Value| chat_req(v);
        // nothing said: each takes its own default, and they differ
        let bare = req(serde_json::json!({ "messages": [] }));
        assert!(bare.image_on(true));
        assert!(!bare.image_on(false));
        assert_eq!(bare.web_mode(), WebMode::Off);
        // images off, search on: the combination one switch could never express
        let split = req(serde_json::json!({
            "messages": [], "image_gen": false, "web_search": "auto"
        }));
        assert!(!split.image_on(true));
        assert_eq!(split.web_mode(), WebMode::Auto);
        // ...and the other way round
        let split = req(serde_json::json!({ "messages": [], "image_gen": "auto" }));
        assert!(split.image_on(false));
        assert_eq!(split.web_mode(), WebMode::Off);
        assert!(!req(serde_json::json!({ "messages": [], "image_gen": "off" })).image_on(true));
    }

    /// A client that declares its OWN tools gets the passthrough: the array
    /// becomes a registry the prompt renders, the boolean extension stays the
    /// deployment-tools switch, and `tool_choice: "none"` still withholds
    /// everything.
    #[test]
    fn client_declared_tools_become_a_registry() {
        let r = chat_req(serde_json::json!({
            "messages": [],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "Current weather for a city.",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            }}],
        }));
        let list = r.client_tools().unwrap().expect("an array is the passthrough");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "get_weather");
        assert!(list[0].parameters["properties"]["city"].is_object());
        // the boolean extension is not an array and must not trip it
        assert!(chat_req(serde_json::json!({ "messages": [], "tools": true }))
            .client_tools().unwrap().is_none());
        assert!(chat_req(serde_json::json!({ "messages": [], "tools": [] }))
            .client_tools().unwrap().is_none());
        // the OpenAI off switch means off in this mode too
        let off = chat_req(serde_json::json!({
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "x"}}],
            "tool_choice": "none",
        }));
        assert!(off.client_tools().unwrap().is_none());
        // a nameless entry is an error the client hears, not prose it waits
        // behind
        assert!(chat_req(serde_json::json!({
            "messages": [], "tools": [{"type": "function", "function": {}}],
        }))
        .client_tools()
        .is_err());
    }

    /// OpenAI tool history renders into the trained chatml forms: the
    /// assistant's calls as `<tool_call>` blocks, results as `<tool_response>`
    /// USER turns (named by matching the call id), consecutive results
    /// sharing one turn.
    #[test]
    fn client_tool_history_folds_into_trained_turns() {
        let r = chat_req(serde_json::json!({ "messages": [
            {"role": "user", "content": "weather in Oslo and Bergen?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function":
                    {"name": "get_weather", "arguments": "{\"city\": \"Oslo\"}"}},
                {"id": "call_2", "type": "function", "function":
                    {"name": "get_weather", "arguments": "{\"city\": \"Bergen\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "12C, rain"},
            {"role": "tool", "tool_call_id": "call_2", "content": "9C, more rain"},
        ]}));
        let mut msgs = r.messages.clone();
        fold_tool_history(&mut msgs);
        assert_eq!(msgs.len(), 3, "two results share one user turn");
        assert_eq!(msgs[1].role, "assistant");
        // the STRING arguments were re-parsed, so the model sees real JSON
        assert!(msgs[1].content.contains("<tool_call>"), "{}", msgs[1].content);
        assert!(msgs[1].content.contains("\"city\":\"Oslo\""), "{}", msgs[1].content);
        assert_eq!(msgs[2].role, "user");
        assert!(msgs[2].content.contains("<tool_response>"));
        assert!(msgs[2].content.contains("get_weather"), "the id resolved to its name");
        assert!(msgs[2].content.contains("more rain"));
        // a turn with none of these shapes passes through untouched
        assert_eq!(msgs[0].content, "weather in Oslo and Bergen?");
    }

    /// The reply side of the passthrough: the call leaves as OpenAI
    /// `tool_calls` with STRING arguments, and the content keeps the
    /// reasoning while losing the call text.
    #[test]
    fn a_call_leaves_as_openai_tool_calls() {
        let text = "<think>\nneed the weather\n</think>\n<tool_call>\n\
                    {\"name\": \"get_weather\", \"arguments\": {\"city\": \"Oslo\"}}\n</tool_call>";
        let calls = tools::parse_calls(text);
        assert_eq!(calls.len(), 1);
        let j = openai_tool_calls(&calls, 0xabc, false);
        assert_eq!(j[0]["type"], "function");
        assert_eq!(j[0]["function"]["name"], "get_weather");
        assert!(j[0]["id"].as_str().unwrap().starts_with("call_"));
        assert!(j[0].get("index").is_none(), "index is the chunk schema's field");
        let args: serde_json::Value =
            serde_json::from_str(j[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "Oslo");
        // the streaming shape addresses calls by index
        assert_eq!(openai_tool_calls(&calls, 0xabc, true)[0]["index"], 0);
        // content: the reasoning survives, the call does not
        assert_eq!(visible_before_call(text), "<think>\nneed the weather\n</think>");
        // the bare-object form was ALL call
        assert_eq!(visible_before_call("{\"name\": \"x\", \"arguments\": {}}"), "");
    }

    /// `tool_choice` can force a call. It arrives as an instruction - this
    /// runtime has no grammar constraint - so all this proves is that the
    /// words reach the block.
    #[test]
    fn a_forced_tool_choice_reaches_the_block() {
        let named = chat_req(serde_json::json!({ "messages": [], "tool_choice":
            {"type": "function", "function": {"name": "get_weather"}} }));
        assert_eq!(named.tool_must_call().as_deref(), Some("get_weather"));
        let any = chat_req(serde_json::json!({ "messages": [], "tool_choice": "required" }));
        assert_eq!(any.tool_must_call().as_deref(), Some(""));
        assert_eq!(chat_req(serde_json::json!({ "messages": [] })).tool_must_call(), None);
        let list = [tools::Tool {
            name: "get_weather".into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            src: tools::ToolSrc::Client,
        }];
        let block = tools::client_system_block(&list, Some("get_weather"));
        assert!(block.contains("<tools>"));
        assert!(block.contains("MUST call `get_weather`"), "{block}");
        assert!(tools::client_system_block(&list, Some("")).contains("MUST respond with"));
    }

    /// The loop runs a call, feeds the result back, and stops when the budget
    /// is spent instead of calling forever.
    #[test]
    fn the_tool_loop_stops_at_its_budget() {
        let tc: tools::ToolsConfig = serde_json::from_value(serde_json::json!({
            "max_calls": 1,
            "http": [{ "name": "t", "url": "https://h/x" }]
        }))
        .unwrap();
        let b = tools::Builtins::default();
        let mut tl = ToolLoop { cfg: &tc, builtins: b, reg: tools::build(&tc, b, &|_| {}),
                                calls: 0, limit_told: false, log: Vec::new() };
        assert!(tl.armed());
        let mut msgs = vec![ChatMsg::text("user", "hi")];
        // a name the registry does NOT have, so the loop is exercised without
        // wasi:http, which does not exist under a native `cargo test`. It is
        // also the interesting failure: a call that cannot run must not take
        // the answer down with it.
        let call = "<tool_call>{\"name\":\"nope\",\"arguments\":{}}</tool_call>";
        // first call: attempted, conversation grows by the call and its result
        assert!(tl.step(call, &mut msgs, &|_| {}, &|_| {}));
        assert_eq!(msgs.len(), 3);
        assert_eq!(tl.calls, 1);
        assert!(msgs[2].content.contains("<tool_response>"));
        // second: over budget, so it is refused with an instruction to answer
        assert!(tl.step(call, &mut msgs, &|_| {}, &|_| {}));
        assert!(msgs[4].content.contains("NOT run"), "{}", msgs[4].content);
        // third: it was already told once - telling it again is an infinite
        // loop, so the reply goes to the user as it stands
        assert!(!tl.step(call, &mut msgs, &|_| {}, &|_| {}));
        // a reply with no call in it never regenerates
        assert!(!tl.step("Here is the answer.", &mut msgs, &|_| {}, &|_| {}));
    }

    /// What the model is actually shown. The signatures have to reach the
    /// system turn, and the stop string has to be armed, or the loop never
    /// gets a call to parse.
    #[test]
    fn tools_reach_the_rendered_prompt() {
        let tok = test_tokenizer();
        let mut cfg = test_config();
        cfg.template = "chatml".into();
        cfg.thinking = false;
        let list = [tools::Tool {
            name: "get_weather".into(),
            description: "Current weather for a city.".into(),
            parameters: serde_json::json!({"type":"object","properties":{"city":{"type":"string"}}}),
            src: tools::ToolSrc::Http(0),
        }];
        let msgs = vec![ChatMsg::text("user", "weather in Oslo?")];
        // the system prompt is rendered into the prompt string, so checking
        // the token count alone would prove nothing - render it directly
        let system = format!("{}{}", cfg.system_prompt, tools::system_block(&list, 3));
        let r = config::render_template("chatml", &system, &[("user".into(), "hi".into())],
                                        config::ThinkTurn::Plain).unwrap();
        assert!(r.prompt.contains("<tools>"), "{}", r.prompt);
        assert!(r.prompt.contains("get_weather"), "{}", r.prompt);
        // ...and the stop string that ends a turn the moment a call completes
        let (_, stops, _) = build_prompt(&cfg, &tok, &msgs, false,
                                        Capabilities::Tools(&list, 3)).unwrap();
        assert!(stops.iter().any(|s| s == "</tool_call>"), "{stops:?}");
        // a deployment with no tools keeps the old stop set and the note
        let (_, stops, _) = build_prompt(&cfg, &tok, &msgs, false, Capabilities::Note).unwrap();
        assert!(!stops.iter().any(|s| s == "</tool_call>"), "{stops:?}");
    }

    #[test]
    fn loop_guard_catches_the_haiku_failure() {
        let g = LoopGuard::new(4);
        // the observed shape: one phrase repeating forever
        let phrase: Vec<u32> = vec![9, 1, 2, 3, 4, 5, 6];
        let mut looping = vec![100, 101, 102]; // some real text first
        for _ in 0..4 {
            looping.extend_from_slice(&phrase);
        }
        assert!(g.tripped(&looping));
        // three repeats is not yet enough
        let mut three = vec![100, 101, 102];
        for _ in 0..3 {
            three.extend_from_slice(&phrase);
        }
        assert!(!g.tripped(&three));
    }

    /// The failure this exists for, reported live 2026-07-30: a thinking model
    /// loops INSIDE its <think> block, the repetition guard ends the reply
    /// there, and the user gets a reasoning block and no answer. The reply is
    /// the only thing they asked for, so the BLOCK is what has to end.
    /// A throwaway tokenizer that can spell the closing tag, which is the only
    /// thing the think guard asks of one. No pre-tokenizer, so the tag maps to
    /// a single id instead of being split into `<`, `/`, `think`, `>`.
    fn think_tokenizer() -> Tokenizer {
        let json = format!(
            r#"{{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],
                "normalizer":null,"pre_tokenizer":null,"post_processor":null,"decoder":null,
                "model":{{"type":"WordLevel","vocab":{{"{}":1,"x":2}},"unk_token":"x"}}}}"#,
            THINK_CLOSE_TEXT.trim()
        );
        Tokenizer::from_bytes(json.as_bytes()).expect("test tokenizer")
    }

    fn out_at<'a>(
        tok: &'a Tokenizer,
        emit: &'a dyn Fn(&str) -> bool,
        stops: &'a [String],
        text: &str,
        tokens: usize,
    ) -> TextOut<'a> {
        let mut out = TextOut::new(tok, emit, stops);
        out.text.push_str(text);
        out.generated = vec![2u32; tokens];
        out
    }

    /// The failure this exists for, reported live 2026-07-30: a thinking model
    /// loops INSIDE its <think> block, the repetition guard ends the reply
    /// there, and the user gets a reasoning block and no answer. The reply is
    /// the only thing they asked for, so the BLOCK is what has to end.
    #[test]
    fn a_loop_inside_the_reasoning_block_closes_the_block_not_the_reply() {
        let tok = think_tokenizer();
        let (emit, stops): (&dyn Fn(&str) -> bool, Vec<String>) = (&|_: &str| true, Vec::new());
        // a reply mid-<think>, nothing written for the user yet
        let mut g = ThinkGuard::new(0, true, &tok); // budget 0 = UNCAPPED
        assert!(g.open, "a force-opened block is tracked even with no budget");
        assert!(!g.close.is_empty(), "the closing tag must be encodable");

        // uncapped means uncapped: length alone never forces the close
        let mut out = out_at(&tok, emit, &stops, "thinking and thinking", 40_000);
        assert!(!g.over(&out), "budget 0 must not close on length");

        // ...but a loop does, once
        assert!(g.take_loop(), "the first trip inside the block is a rescue");
        assert!(g.over(&out), "and the rescue closes the block on the next check");
        assert!(g.forced && g.by_loop);
        assert!(g.note().contains("repeating itself"), "{}", g.note());
        assert!(!g.open, "the block is shut now");

        // a SECOND loop, now outside the block, is a genuinely degenerate reply
        // and must stop it - one rescue per turn
        assert!(!g.take_loop(), "the rescue is spent");
        out.text.push_str(" and the answer");
        assert!(!g.over(&out), "a closed block is not closed twice");
    }

    #[test]
    fn a_loop_with_no_reasoning_block_still_stops_the_reply() {
        let tok = think_tokenizer();
        let (emit, stops): (&dyn Fn(&str) -> bool, Vec<String>) = (&|_: &str| true, Vec::new());
        // think_open false: plain prose, nothing to rescue, caller stops
        let mut g = ThinkGuard::new(4096, false, &tok);
        assert!(!g.open);
        assert!(!g.take_loop(), "no block means no rescue, so the reply ends");
        let out = out_at(&tok, emit, &stops, "looping prose", 9_000);
        assert!(!g.over(&out), "and the budget cannot fire on a block that is not open");
    }

    /// The budget path must keep working exactly as before: spend it with the
    /// block open and the guard closes, with the budget's own message.
    #[test]
    fn the_think_budget_still_closes_a_block_that_overruns() {
        let tok = think_tokenizer();
        let (emit, stops): (&dyn Fn(&str) -> bool, Vec<String>) = (&|_: &str| true, Vec::new());
        let mut g = ThinkGuard::new(100, true, &tok);
        let under = out_at(&tok, emit, &stops, "still reasoning", 99);
        assert!(!g.over(&under), "under budget, nothing happens");
        let over = out_at(&tok, emit, &stops, "still reasoning", 100);
        assert!(g.over(&over));
        assert!(g.forced && !g.by_loop);
        assert!(g.note().contains("think budget of 100"), "{}", g.note());
    }

    /// And a model that closes the block ITSELF is left alone by both paths.
    #[test]
    fn a_block_the_model_closes_itself_is_never_forced() {
        let tok = think_tokenizer();
        let (emit, stops): (&dyn Fn(&str) -> bool, Vec<String>) = (&|_: &str| true, Vec::new());
        let mut g = ThinkGuard::new(10, true, &tok);
        let done = out_at(
            &tok, emit, &stops,
            &format!("reasoned it out{THINK_CLOSE}now the answer"), 5_000,
        );
        assert!(!g.over(&done), "the tag is there, so there is nothing to force");
        assert!(!g.open && !g.forced);
        assert!(!g.take_loop(), "and a later loop is a real stop, not a rescue");
    }

    /// Reported live 2026-07-30: a system prompt told the model to call a web
    /// search tool, this app has none, and the reply was the fake call itself.
    #[test]
    fn a_fabricated_tool_call_yields_the_query_it_wanted() {
        // the exact shape reported
        assert_eq!(
            fabricated_tool_query("<tool_code>\nsearch_tool(query=\"what happened to omar in the wire\")\n</tool_code>").unwrap(),
            "what happened to omar in the wire",
        );
        // the other dialects models reach for
        assert_eq!(
            fabricated_tool_query("<tool_call>{\"name\": \"web_search\", \"arguments\": {\"query\": \"omar little death\"}}</tool_call>").unwrap(),
            "omar little death",
        );
        assert_eq!(
            fabricated_tool_query("```tool_code\nweb_search(query='omar little death')\n```").unwrap(),
            "omar little death",
        );
        assert_eq!(
            fabricated_tool_query("search(\"who killed omar\")").unwrap(),
            "who killed omar",
        );
        // a think block in front of it does not hide it
        assert_eq!(
            fabricated_tool_query("<think>I should search</think>\n<tool_code>search_tool(query=\"omar\")</tool_code>")
                .as_deref(),
            Some("omar"),
        );
    }

    #[test]
    fn a_real_answer_is_never_mistaken_for_a_tool_call() {
        // the case that must not become a web search: someone ASKING about the
        // syntax, and getting a real answer that quotes it
        let lesson = "You write a tool call like this:\n\n```tool_code\nsearch_tool(query=\"x\")\n```\n\n\
                      The model emits that block and the runtime executes it, then feeds the result back.";
        assert_eq!(fabricated_tool_query(lesson), None);
        // ordinary answers
        assert_eq!(fabricated_tool_query("Omar Little is shot by Kenard in season 5."), None);
        assert_eq!(fabricated_tool_query(""), None);
        assert_eq!(fabricated_tool_query("<think>hmm</think>"), None);
        // an invented tool that is not a search is not this feature's business
        assert_eq!(fabricated_tool_query("<tool_code>run_python(code=\"print(1)\")</tool_code>"), None);
        // a call with no usable query
        assert_eq!(fabricated_tool_query("<tool_code>search_tool()</tool_code>"), None);
        assert_eq!(fabricated_tool_query("<tool_code>search_tool(query=\"ab\")</tool_code>"), None);
    }

    #[test]
    fn the_capability_note_only_lands_where_there_is_a_search_leg() {
        let mut cfg = test_config();
        cfg.search = None;
        assert_eq!(with_capability_note(&cfg, "Be helpful."), "Be helpful.");

        // any configured provider will do; the note keys off presence alone
        cfg.search = serde_json::from_str("{\"provider\":\"exa\"}").ok();
        assert!(cfg.search.is_some(), "search config fixture parses");
        let noted = with_capability_note(&cfg, "Be helpful.");
        assert!(noted.starts_with("Be helpful."), "the deployment's own prompt leads");
        assert!(noted.contains("NO tools"), "and the machinery is a footnote after it");
        assert!(noted.contains("Web results"), "naming where results actually appear");
        // an empty system prompt gets the note alone rather than a leading blank
        let bare = with_capability_note(&cfg, "");
        assert!(bare.starts_with("How this app works"), "{bare:?}");
    }

    #[test]
    fn a_title_is_cleaned_of_everything_a_model_wraps_it_in() {
        // the shapes models actually return
        assert_eq!(clean_title("Rust mutex deadlock").unwrap(), "Rust mutex deadlock");
        assert_eq!(clean_title("\"Rust mutex deadlock\"").unwrap(), "Rust mutex deadlock");
        assert_eq!(clean_title("**Title: Rust mutex deadlock**").unwrap(), "Rust mutex deadlock");
        assert_eq!(clean_title("Title: \"Rust mutex deadlock.\"").unwrap(), "Rust mutex deadlock");
        assert_eq!(clean_title("<think>naming it</think>\nRust mutex deadlock").unwrap(),
            "Rust mutex deadlock");
        // whitespace, including a newline the stop string missed
        assert_eq!(clean_title("  Rust   mutex\tdeadlock  ").unwrap(), "Rust mutex deadlock");
        // capitalisation of names and code is the model's business, not ours
        assert_eq!(clean_title("useEffect dependency loop").unwrap(), "useEffect dependency loop");

        // a title that runs long is cut at a WORD boundary
        let long = clean_title("Debugging a deadlock between two mutexes and a bounded channel")
            .unwrap();
        assert!(long.chars().count() <= 48, "{long:?}");
        assert!(!long.ends_with(' ') && !long.contains("  "), "{long:?}");
        assert!(long.starts_with("Debugging a deadlock"), "{long:?}");

        // and a model that writes a paragraph instead of a title is REFUSED,
        // so the caller keeps its own fallback rather than showing an essay
        assert!(clean_title(&"a very long sentence that keeps going ".repeat(4)).is_none());
        assert!(clean_title("").is_none());
        assert!(clean_title("\"\"").is_none());
        assert!(clean_title("   ").is_none());
    }

    #[test]
    fn the_effort_rating_survives_the_router_line() {
        // the shape the router is asked for, rating first
        assert_eq!(parse_effort("EFFORT: low | NO"), Some(Effort::Low));
        assert_eq!(parse_effort("EFFORT: high | SEARCH: rust 1.90 release notes"), Some(Effort::High));
        // and the shapes models actually produce around it
        assert_eq!(parse_effort("**EFFORT:** medium | NO"), Some(Effort::Medium));
        assert_eq!(parse_effort("effort: HIGH"), Some(Effort::High));
        assert_eq!(parse_effort("<think>hmm</think>\nEFFORT: low"), Some(Effort::Low));
        assert_eq!(parse_effort("- EFFORT: \"medium\""), Some(Effort::Medium));
        assert_eq!(parse_effort("EFFORT: simple | NO"), Some(Effort::Low), "synonyms count");
        // a rating that is not there, or is not a rating, leaves the flat budget
        assert_eq!(parse_effort("SEARCH: weather in oslo"), None);
        assert_eq!(parse_effort("EFFORT: whenever"), None);
        assert_eq!(parse_effort(""), None);
        // and the verdict still parses out of the same line
        assert_eq!(
            parse_router_verdict("EFFORT: high | SEARCH: rust 1.90 release notes", true, false),
            Some(RouterVerdict::Search("rust 1.90 release notes".into())),
        );
        // a query that itself contains a pipe survives: only the rating prefix
        // is split off, not every `|` on the line
        assert_eq!(
            parse_router_verdict("EFFORT: medium | SEARCH: ffmpeg concat | filter_complex", true, false),
            Some(RouterVerdict::Search("ffmpeg concat | filter_complex".into())),
        );
        // a plain verdict with no rating is unchanged
        assert_eq!(
            parse_router_verdict("SEARCH: oslo weather", true, false),
            Some(RouterVerdict::Search("oslo weather".into())),
        );
    }

    #[test]
    fn effort_only_ever_spends_less_than_the_configured_budget() {
        let mut cfg = test_config();
        cfg.think_budget = 8000;
        // no effort block: the rating cannot change anything
        cfg.effort = None;
        assert_eq!(Effort::Low.budget(&cfg), 8000);

        cfg.effort = Some(config::EffortConfig { low: 512, medium: 4096, high: 0, floor: 256 });
        assert_eq!(Effort::Low.budget(&cfg), 512);
        assert_eq!(Effort::Medium.budget(&cfg), 4096);
        assert_eq!(Effort::High.budget(&cfg), 8000, "high 0 = the model's own budget");

        // a class configured ABOVE the model's ceiling is clamped to it: this
        // knob spends less, never more
        cfg.effort = Some(config::EffortConfig { low: 512, medium: 99_000, high: 0, floor: 256 });
        assert_eq!(Effort::Medium.budget(&cfg), 8000);

        // the floor protects a misrated question from losing its reasoning
        cfg.effort = Some(config::EffortConfig { low: 8, medium: 4096, high: 0, floor: 256 });
        assert_eq!(Effort::Low.budget(&cfg), 256);

        // an uncapped model stays uncapped where the class says 0, and takes
        // the class's own number where it gives one
        cfg.think_budget = 0;
        cfg.effort = Some(config::EffortConfig { low: 512, medium: 4096, high: 0, floor: 256 });
        assert_eq!(Effort::High.budget(&cfg), 0, "uncapped stays uncapped");
        assert_eq!(Effort::Low.budget(&cfg), 512, "but chit-chat still gets a ceiling");
    }

    #[test]
    fn loop_guard_leaves_ordinary_text_alone() {
        let g = LoopGuard::new(4);
        // prose: no exact repeating block
        let prose: Vec<u32> = (0..500).map(|i| (i * 7919 % 4001) as u32).collect();
        assert!(!g.tripped(&prose));
        // a short run of one token (a rule, indentation, "...") is normal and
        // must survive - the guard demands far more evidence for period 1
        let mut dashes = vec![5u32; 12];
        dashes.splice(0..0, [1, 2, 3]);
        assert!(!g.tripped(&dashes));
        // ...but a token repeated forever is still a loop
        assert!(g.tripped(&vec![5u32; 200]));
        // a two-token alternation needs more than four cycles too
        let short: Vec<u32> = std::iter::repeat([7u32, 8]).take(5).flatten().collect();
        assert!(!g.tripped(&short));
        assert!(g.tripped(&std::iter::repeat([7u32, 8]).take(40).flatten().collect::<Vec<_>>()));
    }

    #[test]
    fn loop_guard_off_when_zero() {
        assert!(!LoopGuard::new(0).tripped(&vec![5u32; 500]));
    }

    #[test]
    fn internal_status_narrates_the_queue_and_gives_up_on_budget() {
        use std::cell::RefCell;
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let sink = |s: &str| seen.borrow_mut().push(s.to_string());
        let relay = internal_status("deciding what this needs…", &sink, 60_000);
        // the load/ready lines are swallowed, not forwarded - and never abort
        assert!(relay("loading the model on gpu - the first request after a node boot..."));
        assert!(relay("session ready (3 ms); prefilling 900 prompt tokens"));
        assert!(seen.borrow().is_empty());
        // a queue tick is forwarded under the leg's own label: these bytes are
        // what keeps the gateway from cutting the stream mid-wait
        assert!(relay(&format!("{BUSY_STATUS} (2s) - waiting for a free slot")));
        let got = seen.borrow().last().cloned().unwrap();
        assert!(got.starts_with("deciding what this needs…"), "{got}");
        assert!(got.contains("waiting for a free inference slot"), "{got}");
        // budget spent: the relay says stop, and nothing more is forwarded
        let spent = internal_status("deciding what this needs…", &sink, 0);
        let n = seen.borrow().len();
        assert!(!spent(&format!("{BUSY_STATUS} (31s) - waiting for a free slot")));
        assert_eq!(seen.borrow().len(), n);
        // but even with no budget, non-queue lines still pass without aborting
        assert!(spent("loading the model on cpu - ..."));
    }

    #[test]
    fn router_verdict_survives_model_noise() {
        // the clean cases
        assert_eq!(search_of("SEARCH: rust 1.90 release notes").as_deref(),
                   Some("rust 1.90 release notes"));
        assert_eq!(parse_router_verdict("NO", true, true), None);
        assert_eq!(parse_router_verdict("NO\n", true, true), None);
        // the noise models actually emit
        assert_eq!(search_of("**SEARCH:** \"btc price\"").as_deref(), Some("btc price"));
        assert_eq!(search_of("- search: weather in oslo").as_deref(), Some("weather in oslo"));
        assert_eq!(search_of("<think>\nneeds fresh data\n</think>\nSEARCH: nvidia stock")
                       .as_deref(), Some("nvidia stock"));
        assert_eq!(search_of("Sure!\nSEARCH: who won the 2026 world cup").as_deref(),
                   Some("who won the 2026 world cup"));
        // degenerate: a SEARCH with no query is not a search
        assert_eq!(parse_router_verdict("SEARCH:", true, true), None);
        assert_eq!(parse_router_verdict("SEARCH:   \"\"  ", true, true), None);
        // a refusal that merely mentions the word must not trigger one
        assert_eq!(parse_router_verdict("I do not need to search for this.", true, true), None);
    }

    #[test]
    fn image_verdict_is_gated_on_the_capability() {
        assert_eq!(image_of("IMAGE: a watercolour fox in a snowy forest").as_deref(),
                   Some("a watercolour fox in a snowy forest"));
        assert_eq!(image_of("**IMAGE:** \"a red bicycle\"").as_deref(), Some("a red bicycle"));
        // a model that invents the capability on a deployment without an
        // image service must be ignored, not obeyed
        assert_eq!(parse_router_verdict("IMAGE: a red bicycle", true, false), None);
        // ...but a SEARCH on the same reply still counts
        assert_eq!(
            match parse_router_verdict("IMAGE: a fox\nSEARCH: fox facts", true, false) {
                Some(RouterVerdict::Search(q)) => q,
                other => panic!("{other:?}"),
            },
            "fox facts"
        );
        // image prompts keep more room than search queries
        let long = "x".repeat(900);
        assert_eq!(image_of(&format!("IMAGE: {long}")).unwrap().chars().count(), 900);
    }

    #[test]
    fn web_mode_accepts_both_shapes() {
        let mk = |v: serde_json::Value| -> ChatReq {
            serde_json::from_value(serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}], "web_search": v
            })).unwrap()
        };
        assert!(mk(serde_json::json!(true)).web_mode() == WebMode::Always);
        assert!(mk(serde_json::json!("auto")).web_mode() == WebMode::Auto);
        assert!(mk(serde_json::json!("AUTO")).web_mode() == WebMode::Auto);
        assert!(mk(serde_json::json!(false)).web_mode() == WebMode::Off);
        assert!(mk(serde_json::json!("nonsense")).web_mode() == WebMode::Off);
        // absent entirely
        let bare: ChatReq = serde_json::from_value(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}]
        })).unwrap();
        assert!(bare.web_mode() == WebMode::Off);
    }

    #[test]
    fn search_prefix_is_stripped_only_when_it_is_a_command() {
        assert_eq!(strip_search_prefix("/search rust wasm").as_deref(), Some("rust wasm"));
        assert_eq!(strip_search_prefix("  /web  rust wasm ").as_deref(), Some("rust wasm"));
        assert_eq!(strip_search_prefix("/SEARCH Rust").as_deref(), Some("Rust"));
        // not commands: no argument, no delimiter, or merely mentioned
        assert_eq!(strip_search_prefix("/search"), None);
        assert_eq!(strip_search_prefix("/search   "), None);
        assert_eq!(strip_search_prefix("/searching for a job"), None);
        assert_eq!(strip_search_prefix("what does /search do?"), None);
        assert_eq!(strip_search_prefix("search the web for rust"), None);
    }

    #[test]
    fn image_prefix_is_its_own_command() {
        assert_eq!(strip_image_prefix("/image a red fox").as_deref(), Some("a red fox"));
        assert_eq!(strip_image_prefix("/img a red fox").as_deref(), Some("a red fox"));
        assert_eq!(strip_image_prefix("/DRAW a red fox").as_deref(), Some("a red fox"));
        assert_eq!(strip_image_prefix("/imagine a red fox"), None);
        assert_eq!(strip_image_prefix("/image"), None);
        // the two command families do not overlap
        assert_eq!(strip_search_prefix("/image a red fox"), None);
        assert_eq!(strip_image_prefix("/search rust"), None);
    }

    // ---------------------------------------------------------- vision --

    /// The smallest valid PNG header the sniffer accepts, padded past the
    /// 12-byte minimum.
    fn png_bytes() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(b"IHDRxxxx");
        v
    }

    fn b64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn base64_roundtrips_and_rejects_junk() {
        let img = png_bytes();
        assert_eq!(b64_decode(&b64(&img)).unwrap(), img);
        // padding is optional and whitespace is ignored (hand-built requests)
        let padded = b64(&img);
        let loose = format!("{}\n {}", &padded[..8], &padded[8..].replace('=', ""));
        assert_eq!(b64_decode(&loose).unwrap(), img);
        assert!(b64_decode("not base64!").is_err());
        assert!(b64_decode("").is_err());
    }

    #[test]
    fn the_open_chat_is_addressed_by_path() {
        // what the playground writes into the URL bar
        assert!(is_chat_path("/c/c1a2b3c4d5e"));
        assert!(is_chat_path("/c/c1a2b3c4d5e/"));
        assert!(is_chat_path("/c"), "the bare prefix lands on a new chat");
        assert!(is_chat_path("/c/"));
        // and what must NOT quietly serve a broken page
        assert!(!is_chat_path("/c/a/b"), "one segment only; the page's base strips one");
        assert!(!is_chat_path("/cx"));
        assert!(!is_chat_path("/chat"), "the POST route, not a chat address");
        assert!(!is_chat_path("/models"));
        assert!(!is_chat_path("/"));
        assert!(!is_chat_path(""));
    }

    #[test]
    fn image_formats_are_sniffed_not_trusted() {
        assert_eq!(image_kind(&png_bytes()), Some("png"));
        assert_eq!(image_kind(&[0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0]), Some("jpeg"));
        let mut webp = b"RIFF0000WEBPVP8 ".to_vec();
        webp.truncate(16);
        assert_eq!(image_kind(&webp), Some("webp"));
        assert_eq!(image_kind(b"GIF89a000000"), Some("gif"));
        // a mislabelled upload is caught here, not in the vision encoder
        assert_eq!(image_kind(b"<html>not an image"), None);
        assert_eq!(image_kind(b"short"), None);
    }

    #[test]
    fn a_webp_upload_reaches_the_encoder_as_jpeg() {
        // stb_image inside mtmd has no VP8, so bytes that arrive as webp must
        // leave this function as something the encoder can actually read. The
        // failure this prevents was a live one: the host refused the picture
        // with a message that listed webp among the formats it reads.
        let mut src = Vec::new();
        for _ in 0..(24 * 24) {
            src.extend_from_slice(&[7, 130, 255]);
        }
        let mut wp = Vec::new();
        image_webp::WebPEncoder::new(&mut wp)
            .encode(&src, 24, 24, image_webp::ColorType::Rgb8)
            .unwrap();
        assert_eq!(image_kind(&wp), Some("webp"));
        let out = decode_image_src(&format!("data:image/webp;base64,{}", b64(&wp))).unwrap();
        assert_eq!(image_kind(&out), Some("jpeg"), "webp must not survive the door");
        // every other format is passed through untouched
        assert_eq!(decode_image_src(&format!("data:image/png;base64,{}", b64(&png_bytes()))).unwrap(),
            png_bytes());
    }

    #[test]
    fn image_src_takes_data_uris_and_refuses_remote_urls() {
        let uri = format!("data:image/png;base64,{}", b64(&png_bytes()));
        assert_eq!(decode_image_src(&uri).unwrap(), png_bytes());
        // bare base64 (the Anthropic content shape) needs no envelope
        assert_eq!(decode_image_src(&b64(&png_bytes())).unwrap(), png_bytes());
        // a remote URL is refused rather than fetched: fetching would tell a
        // third party what this deployment is looking at
        let e = decode_image_src("https://example.com/cat.png").unwrap_err();
        assert!(e.contains("not fetched"), "{e}");
        // and a data: URI holding something that is not an image
        let junk = format!("data:image/png;base64,{}", b64(b"<html>nope, plain text"));
        assert!(decode_image_src(&junk).is_err());
    }

    #[test]
    fn content_parts_parse_in_all_three_spellings() {
        let uri = format!("data:image/png;base64,{}", b64(&png_bytes()));
        let openai: ChatMsg = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": uri}},
            ]
        })).unwrap();
        assert_eq!(openai.content, "what is this");
        assert_eq!(openai.images.len(), 1);
        assert_eq!(openai.images[0], png_bytes());

        let responses: ChatMsg = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [{"type": "input_image", "image_url": format!("data:image/png;base64,{}", b64(&png_bytes()))}]
        })).unwrap();
        assert_eq!(responses.images.len(), 1);

        let anthropic: ChatMsg = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                     "data": b64(&png_bytes())}}]
        })).unwrap();
        assert_eq!(anthropic.images.len(), 1);

        // and the plain string form is untouched
        let plain: ChatMsg = serde_json::from_value(serde_json::json!({
            "role": "user", "content": "just words"
        })).unwrap();
        assert_eq!(plain.content, "just words");
        assert!(plain.images.is_empty());
    }

    #[test]
    fn media_marks_cannot_be_forged_by_a_message() {
        // a message that arrives carrying our private marker must not be able
        // to claim an image slot: the mark is stripped before rendering
        let sneaky = format!("look{}here", config::MEDIA_MARK);
        assert_eq!(strip_marks(&sneaky), "lookhere");
        assert!(!strip_marks(&sneaky).contains(config::MEDIA_MARK));
    }

    #[test]
    fn rendered_prompt_splits_around_its_images() {
        let tok = test_tokenizer();
        let rendered = format!("before {} after", config::MEDIA_MARK);
        let p = split_rendered(&tok, &rendered, vec![png_bytes()]).unwrap();
        assert_eq!(p.images, 1);
        assert_eq!(p.parts.len(), 3);
        assert!(matches!(p.parts[0], PromptPart::Text(_)));
        assert!(matches!(p.parts[1], PromptPart::Image(_)));
        assert!(matches!(p.parts[2], PromptPart::Text(_)));
        // the flat token stream is the text runs, in order, and nothing else
        let joined: Vec<u32> = p.parts.iter().filter_map(|x| match x {
            PromptPart::Text(t) => Some(t.clone()),
            _ => None,
        }).flatten().collect();
        assert_eq!(joined, p.text_ids);
        // text-only prompts stay exactly what they were
        let plain = split_rendered(&tok, "no pictures here", vec![]).unwrap();
        assert!(plain.text_only().is_some());
        assert!(p.text_only().is_none());
        // slot/image count mismatches are caught rather than silently misaligned
        assert!(split_rendered(&tok, &rendered, vec![]).is_err());
        assert!(split_rendered(&tok, "no slot", vec![png_bytes()]).is_err());
    }

    #[test]
    fn an_image_at_the_end_leaves_no_dangling_slot() {
        let tok = test_tokenizer();
        let rendered = format!("caption this {}", config::MEDIA_MARK);
        let p = split_rendered(&tok, &rendered, vec![png_bytes()]).unwrap();
        // trailing empty chunk contributes no part
        assert_eq!(p.parts.len(), 2);
        assert!(matches!(p.parts[1], PromptPart::Image(_)));
    }

    #[test]
    fn a_projector_is_not_mistaken_for_a_model() {
        // both sides of the boundary use this rule: the app to pick the
        // weights file out of a vision volume, the host to pick the projector
        assert!(is_mmproj(Path::new("/models/vl/mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf")));
        assert!(is_mmproj(Path::new("/models/vl/MMPROJ-model-f16.gguf")));
        assert!(!is_mmproj(Path::new("/models/vl/model.gguf")));
        assert!(!is_mmproj(Path::new("/models/vl/Qwen3VL-8B-Instruct-Q4_K_M.gguf")));
    }

    #[test]
    fn images_are_refused_by_models_that_cannot_see() {
        let raw = serde_json::json!({});
        let mut cfg = test_config();
        let msgs = vec![ChatMsg {
            role: "user".into(),
            content: "what is this".into(),
            images: vec![png_bytes()],
            ..ChatMsg::default()
        }];
        // text-only model: refused, and the message says which knob is wrong
        cfg.vision = false;
        let e = check_images(&raw, &cfg, &msgs).unwrap_err();
        assert!(e.starts_with("[no_vision]"), "{e}");
        // the onnx backend has no image verb at all, vision flag or not
        cfg.vision = true;
        cfg.backend = "onnx".into();
        assert!(check_images(&raw, &cfg, &msgs).is_err());
        // a vision model on ggml accepts it
        cfg.backend = "ggml".into();
        assert_eq!(check_images(&raw, &cfg, &msgs).unwrap(), 1);
        // ...within the deployment's limits
        cfg.max_images = 1;
        let two = vec![msgs[0].clone(), msgs[0].clone()];
        assert!(check_images(&raw, &cfg, &two).unwrap_err().starts_with("[too_many_images]"));
        cfg.max_images = 4;
        cfg.max_image_bytes = 4;
        assert!(check_images(&raw, &cfg, &msgs).unwrap_err().starts_with("[image_too_large]"));
        // a text-only request never touches any of this
        cfg.vision = false;
        assert_eq!(check_images(&raw, &cfg, &[ChatMsg::text("user", "hi")]).unwrap(), 0);
    }

    fn vision_service() -> vision::VisionConfig {
        serde_json::from_value(serde_json::json!({ "endpoint": "https://eye.app.enclave.host" }))
            .unwrap()
    }

    #[test]
    fn a_vision_service_lets_a_text_only_model_take_pictures() {
        let raw = serde_json::json!({});
        let mut cfg = test_config();
        cfg.vision = false;
        let msgs = vec![ChatMsg {
            role: "user".into(),
            content: "what is this".into(),
            images: vec![png_bytes()],
            ..ChatMsg::default()
        }];
        // without a service this is the refusal asserted above...
        assert!(check_images(&raw, &cfg, &msgs).is_err());
        // ...with one, the turn is accepted and the sibling deployment reads it
        cfg.vision_service = Some(vision_service());
        assert_eq!(check_images(&raw, &cfg, &msgs).unwrap(), 1);
        // the per-request limits still bind - they are about THIS app's window
        cfg.max_image_bytes = 4;
        assert!(check_images(&raw, &cfg, &msgs).unwrap_err().starts_with("[image_too_large]"));
    }

    #[test]
    fn who_reads_the_picture() {
        let mut cfg = test_config();
        cfg.backend = "ggml".into(); // the only backend with an image verb
        // no service configured: always the serving model, whatever it can do
        cfg.vision = true;
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Local);
        cfg.vision = false;
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Local);

        cfg.vision_service = Some(vision_service());
        // a text-only serving model has nothing to decide
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Delegate);
        // a serving model that CAN see still delegates by default: that is what
        // configuring a service means
        cfg.vision = true;
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Delegate);
        // ...unless the request NAMED it, by model name or by volume
        assert_eq!(vision_plan(&cfg, Some(&cfg.name.clone())), VisionPlan::Local);
        assert_eq!(vision_plan(&cfg, Some(&cfg.model_volume.clone())), VisionPlan::Local);
        // naming some OTHER model is not an instruction about this one
        assert_eq!(vision_plan(&cfg, Some("something-else")), VisionPlan::Delegate);
        // ...or unless the deployment asked for local reads
        cfg.vision_service.as_mut().unwrap().prefer_local = true;
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Local);
        // prefer_local cannot conjure a capability the model does not have
        cfg.vision = false;
        assert_eq!(vision_plan(&cfg, None), VisionPlan::Delegate);
    }

    #[test]
    fn the_authored_question_survives_model_noise() {
        assert_eq!(
            parse_ask_line("ASK: What does the error dialog say, exactly?").as_deref(),
            Some("What does the error dialog say, exactly?")
        );
        // a think block, a bullet, bold, quotes, a lowercase prefix
        assert_eq!(
            parse_ask_line("<think>hmm</think>\n- **ask:** \"Read the total.\""),
            Some("Read the total.".to_string())
        );
        // no prefix at all, but it is plainly a question
        assert_eq!(
            parse_ask_line("Which of the three buttons is disabled?").as_deref(),
            Some("Which of the three buttons is disabled?")
        );
        // commentary is not a question, and nothing usable means fall back
        assert_eq!(parse_ask_line("Sure, I can help with that."), None);
        assert_eq!(parse_ask_line("ASK:"), None);
        assert_eq!(parse_ask_line(""), None);
    }

    #[test]
    fn a_delegated_turn_ends_up_with_no_image_bytes_and_a_report() {
        // the fold itself, without the network: render_context + the rewrite
        // rules apply_vision uses
        let a = vision::VisionAnswer {
            text: "A red barn beside a fence.".into(),
            question: "What is in this photo?".into(),
            model: Some("qwen3-vl-8b".into()),
            images: 1,
            image_tokens: 258,
            ms: 1200,
            truncated: false,
        };
        let report = vision::render_context(&a);
        let folded = format!("{report}\nQuestion: what is this");
        // the model is told it did not see the picture, and the user's own
        // question is the last thing in the turn
        assert!(folded.contains("VISION REPORT"));
        assert!(folded.trim_end().ends_with("Question: what is this"));
        assert!(folded.contains("A red barn beside a fence."));
    }
}
