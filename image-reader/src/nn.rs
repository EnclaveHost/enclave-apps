//! The inference half: model volumes, the ggml session, the image verb, and
//! one generation.
//!
//! BACKEND: ggml (llama.cpp) only, unlike the sibling llm-chat app which also
//! carries an ONNX path. That is not a simplification for its own sake - the
//! ONNX path shuttles the KV cache through guest memory as tensors and has no
//! image verb at all, so there is nothing for a vision app to fall back to.
//! The weights are HOST-PRELOADED from the attached volume
//! (-S nn-graph=ggml::<volume dir>), so load_by_name() never pulls them into
//! guest memory and model size is bounded by the deployment's GPU share rather
//! than by wasm32's address space.
//!
//! WHAT THE HOST DOES WITH AN IMAGE: the bytes cross as the FILE, not as
//! pixels. Decoding, resizing, the vision encoder, the projector and the
//! model's own marker tokens all live behind the host's `image` verb, which
//! splices the encoded result into the sequence and answers with the POSITIONS
//! it consumed. So this app carries no image-format code whatsoever - no PNG
//! decoder, no resampler - and a new VLM architecture is a llama.cpp change,
//! not a change here.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokenizers::Tokenizer;

use crate::bindings;
use crate::bindings::wasi::nn::graph::load_by_name;
use crate::bindings::wasi::nn::inference::GraphExecutionContext;
use crate::bindings::wasi::nn::tensor::{Tensor, TensorType};
use crate::config::{self, AppConfig};
use crate::sampling::{pick_token, Rng, SampleParams};

// ------------------------------------------------------------ model volumes --
// Weights, projector and tokenizer arrive as ATTACHED MODEL VOLUMES (Tinfoil
// Modelwrap): the platform preopens each attached volume read-only at
// /models/<name> and lists the names in ENCLAVE_MODELS. The app embeds NO
// weights - the config's `models` catalog describes the volumes it can serve,
// so ONE published wasm serves whatever the deployment mounts.

pub const MODELS_ROOT: &str = "/models";

pub fn attached_volumes() -> Vec<String> {
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
/// tenant log); on an unlisted name the host never tried (volume mounted late,
/// over the VRAM budget, or no unambiguous file). None = the manager predates
/// the env, so there is no signal and the diagnosis stays generic.
pub fn preloaded_graphs() -> Option<Vec<String>> {
    std::env::var("ENCLAVE_NN_PRELOADS").ok().map(|v| {
        v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
    })
}

/// Resolve a config path against the model volume: absolute paths are used
/// verbatim (a cross-VOLUME read, e.g. a tokenizer living in a sibling volume
/// when the weights repo carries none), everything else is volume-relative.
fn volume_path(root: &PathBuf, rel: &str) -> PathBuf {
    if rel.starts_with('/') {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    }
}

pub fn read_tokenizer(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    if !root.is_dir() {
        let have = attached_volumes();
        return Err(format!(
            "model volume '{}' is not attached at {MODELS_ROOT}/{} (attached: {}) - deploy \
             with {{\"volumes\":[\"{}\"]}} in the config, or tick it in the console's volume \
             picker",
            cfg.model_volume,
            cfg.model_volume,
            if have.is_empty() { "none".to_string() } else { have.join(", ") },
            cfg.model_volume,
        ));
    }
    let mut rels: Vec<String> = Vec::new();
    rels.extend(cfg.tokenizer_file.iter().cloned());
    rels.push("tokenizer.json".into());
    for rel in &rels {
        let p = volume_path(&root, rel);
        if p.is_file() {
            return std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display()));
        }
    }
    Err(format!(
        "no tokenizer.json in volume '{}' (tried: {}) - a ggml volume needs one because the \
         GUEST tokenizes the prompt; point tokenizer_file at another volume's copy if the \
         weights repo ships none",
        cfg.model_volume,
        rels.join(", ")
    ))
}

/// Does this file name mark a vision projector rather than a model? The HOST
/// picks the projector out of a volume by exactly this convention, so the two
/// sides must agree or they will disagree about which gguf is the model.
pub fn is_mmproj(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("mmproj"))
        .unwrap_or(false)
}

/// The vision projector's size in the model volume, if it carries one. `None`
/// here is the single most useful fact this app can report about a volume: it
/// means the volume holds a language model and nothing to see with.
pub fn mmproj_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false) && is_mmproj(p))
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
        .max()
}

/// The split-GGUF family covering `path` (llama.cpp's
/// "<prefix>-NNNNN-of-MMMMM.gguf" convention, forced on large models by HF's
/// per-file cap), every part present - or None if `path` isn't a split part or
/// a sibling is missing.
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
    let parts: Vec<PathBuf> =
        (1..=n).map(|i| dir.join(format!("{prefix}-{i:05}-of-{count}.gguf"))).collect();
    parts.iter().all(|p| p.is_file()).then_some(parts)
}

/// Locate `cfg`'s weights file WITHOUT reading it: powers the availability
/// listing and the size ranking that picks the default model. Mirrors the
/// lookup the host does for real - model.gguf, else the volume's single gguf,
/// else one complete split family - with the PROJECTOR taken out of the pick
/// first. That exclusion is the whole reason a vision volume can hold two
/// ggufs and still be unambiguous.
pub fn weights_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
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
                .filter(|p| !is_mmproj(p))
                .collect();
            match ggufs.len() {
                0 => return None,
                1 => ggufs.pop()?,
                n => ggufs
                    .iter()
                    .find(|p| split_family(p).is_some_and(|fam| fam.len() == n))?
                    .clone(),
            }
        }
    };
    let parts = split_family(&path);
    let files = parts.as_deref().unwrap_or(std::slice::from_ref(&path));
    let mut total = 0u64;
    for f in files {
        total += std::fs::metadata(f).ok()?.len();
    }
    Some(total)
}

// ------------------------------------------------------------ model choice --

/// One servable model: an attached volume the config knows how to describe.
pub struct ModelEntry {
    pub volume: String,
    pub bytes: u64,
    /// the projector's size, or None when the volume carries none - which in
    /// this app means the volume cannot do the one thing it was attached for
    pub mmproj: Option<u64>,
    pub cfg: AppConfig,
}

/// The servable models: every attached volume with a `models` catalog entry (or
/// equal to the top-level model_volume). Sorted by weights size, LARGEST FIRST
/// - index 0 is the default, so a deployment that attaches several serves the
/// biggest unless a request names another. Volumes that are attached but
/// undescribed (unknown geometry/template) or missing their weights are
/// skipped: they cannot be served, only misserved.
///
/// A volume with no projector is NOT skipped. It is listed with mmproj: None
/// so /health and /v1/models can say what is wrong with it, which is the
/// difference between an operator seeing "attached but has no projector" and
/// seeing nothing at all.
pub fn available_models(raw: &serde_json::Value) -> Vec<ModelEntry> {
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
        let mmproj = mmproj_size(&cfg);
        out.push(ModelEntry { volume: vol, bytes, mmproj, cfg });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.volume.cmp(&b.volume)));
    out
}

/// The deployment's VRAM budget in bytes: ENCLAVE_VRAM_BYTES, set by the
/// platform from gpuShare x card VRAM - the same number the MPS cap enforces
/// on this process. None on CPU deployments and older managers.
pub fn vram_budget() -> Option<u64> {
    std::env::var("ENCLAVE_VRAM_BYTES").ok()?.trim().parse::<u64>().ok().filter(|b| *b > 0)
}

/// Whether this deployment holds GPU resources at all. ENCLAVE_VRAM_BYTES is
/// set on every GPU deployment and left unset on a CPU one, so a value is
/// PROOF of a share and its absence INSIDE a tenant environment is the CPU
/// signal. Some(true) = GPU share; Some(false) = tenant with no GPU;
/// None = not a platform tenant (a dev box), and nothing is known.
pub fn gpu_present() -> Option<bool> {
    if vram_budget().is_some() {
        return Some(true);
    }
    std::env::vars().any(|(k, _)| k.starts_with("ENCLAVE_")).then_some(false)
}

/// Bytes per KV-cache element, in SIXTEENTHS (q8_0 stores 34 bytes per
/// 32-element block = 17/16 bytes each; f16 = 32/16).
fn kv_elem_sixteenths(t: &str) -> u64 {
    match t {
        "f32" => 64,
        "q8_0" => 17,
        "q4_0" => 9,
        "q4_1" => 10,
        _ => 32, // f16 / bf16 / unknown
    }
}

/// The manager sets ENCLAVE_GGML_POOLED on hosts whose ggml backend serves all
/// concurrent sessions from one shared KV pool per model (continuous batching).
/// It changes the serve-cost SHAPE: pools price once per model and persist once
/// a model has served, instead of transient per-request windows.
fn pooled_backend() -> bool {
    std::env::var("ENCLAVE_GGML_POOLED")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

const WORKING_SET: u64 = 3 << 29; // 1.5 GiB, deliberately round

/// Estimated VRAM to SERVE one model beyond its resident weights: the KV cache
/// at the NODE's context window, the projector, and a flat working-set
/// allowance (compute buffers, FA workspace, CUDA context).
///
/// llama.cpp allocates the FULL window up front regardless of prompt length
/// and does not clamp to the model's training window, so the window term
/// dominates - a 176K node window on a dense 36-layer model is many GB whether
/// or not anyone sends a long prompt.
///
/// THE PROJECTOR IS PRICED HERE, and that is the point of this function in
/// this app: it loads LAZILY, on the first image the deployment is sent. A
/// share that fits the language model and nothing else would start fine,
/// answer /health fine, and then die on the first picture - and a CUDA OOM
/// inside compute calls ggml_abort, which takes the whole wasmtime process
/// down, every model with it, with no error reaching the guest. So a
/// deployment that cannot afford the projector must be told BEFORE it serves.
///
/// Returns (kv_and_projector, working_set) - callers sum them.
pub fn serve_cost(cfg: &AppConfig) -> (u64, u64) {
    let Some(n_ctx) = std::env::var("ENCLAVE_GGML_N_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    else {
        // unknown node tuning (older manager, dev box): degrade to
        // weights-only rather than invent a window
        return (0, 0);
    };
    let tk = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").unwrap_or_default();
    let tv = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").unwrap_or_else(|_| tk.clone());
    let elems =
        cfg.kv_layers.unwrap_or(cfg.n_layers) as u64 * cfg.n_kv_heads as u64 * cfg.head_dim as u64;
    let kv = elems * n_ctx * (kv_elem_sixteenths(tk.trim()) + kv_elem_sixteenths(tv.trim())) / 16;
    // the projector's real size is knowable (the file is right there); the
    // allowance on top covers the encode workspace. A volume with no projector
    // still gets the allowance, because this app will try to load one and the
    // failure should not be an OOM.
    let vision = mmproj_size(cfg).unwrap_or(1 << 30) + (1 << 29);
    if pooled_backend() {
        return (kv + vision, WORKING_SET);
    }
    let sessions = std::env::var("ENCLAVE_GGML_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    (kv * sessions + vision, WORKING_SET * sessions)
}

/// Which servable models CANNOT serve within the VRAM budget. Models claim the
/// budget SMALLEST-FIRST (the preload / warmup order, same tie-break as the
/// manager's emission), each needing its weights resident plus serve_cost.
/// Refusing here is load-bearing rather than cosmetic: a too-big model must
/// never be probed, let alone served, because the OOM aborts the tenant.
/// Returns volume -> reason, unfit models only.
pub fn over_budget(entries: &[ModelEntry]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(budget) = vram_budget() else { return out };
    let mut asc: Vec<&ModelEntry> = entries.iter().collect();
    asc.sort_by(|a, b| a.bytes.cmp(&b.bytes).then(a.volume.cmp(&b.volume)));
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut claimed = 0u64;
    for e in asc {
        let (kv, ws) = serve_cost(&e.cfg);
        if claimed + e.bytes + kv + ws <= budget {
            claimed += e.bytes + if pooled_backend() { kv + ws } else { 0 };
            continue;
        }
        let need = e.bytes + kv + ws;
        out.insert(
            e.volume.clone(),
            if kv > 0 {
                format!(
                    "needs ~{:.1} GB to serve ({:.1} GB weights + {:.1} GB KV cache at the \
                     node's context window + {:.1} GB projector and working set) but {:.1} GB \
                     of the {:.1} GB VRAM budget remains - redeploy with a larger GPU share",
                    gb(need),
                    gb(e.bytes),
                    gb(kv.saturating_sub(e.mmproj.unwrap_or(0))),
                    gb(e.mmproj.unwrap_or(0) + ws),
                    gb(budget.saturating_sub(claimed)),
                    gb(budget)
                )
            } else {
                format!(
                    "{:.1} GB of weights cannot fit the deployment's {:.1} GB VRAM budget{} - \
                     redeploy with a larger GPU share",
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
    out
}

/// The AppConfig serving one request. `requested` (the OpenAI `model` field, or
/// ?model=) matches a model name or a volume name; an UNKNOWN name falls back
/// to the default instead of erroring, because OpenAI SDKs require a model
/// string and clients routinely send one this deployment never heard of. A
/// KNOWN name the VRAM budget cannot serve is REFUSED with the reason.
pub fn resolve_model(
    raw: &serde_json::Value,
    requested: Option<&str>,
) -> Result<AppConfig, String> {
    let entries = available_models(raw);
    let unfit = over_budget(&entries);
    if let Some(want) = requested.map(str::trim).filter(|w| !w.is_empty()) {
        if let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) {
            if let Some(why) = unfit.get(&e.volume) {
                return Err(format!("model '{want}' cannot serve on this deployment: {why}"));
            }
            return Ok(e.cfg.clone());
        }
    }
    // prefer a model that fits AND can see: a projector-less volume is worse
    // than useless here, so it is the last thing this app will pick
    if let Some(e) =
        entries.iter().find(|e| !unfit.contains_key(&e.volume) && e.mmproj.is_some())
    {
        return Ok(e.cfg.clone());
    }
    if let Some(e) = entries.iter().find(|e| !unfit.contains_key(&e.volume)) {
        return Ok(e.cfg.clone());
    }
    match entries.into_iter().next() {
        Some(e) => Ok(e.cfg),
        None => config::from_value(raw.clone()),
    }
}

// ------------------------------------------------------------------ clocks --

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Sleep on the monotonic clock's pollable - parks this request in the host's
/// poll loop (a spin wait would burn the share's cpu slice for nothing).
fn sleep_ms(ms: u64) {
    let p = bindings::wasi::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000);
    bindings::wasi::io::poll::poll(&[&p]);
}

/// The host's ggml session gate (ENCLAVE_GGML_MAX_SESSIONS) tags its fail-fast
/// error with this marker. Every llama context pre-allocates the FULL n_ctx KV
/// window, so the host caps live contexts and this app QUEUES instead of
/// stacking window allocations until the share's VRAM dies.
const BUSY_MARKER: &str = "[sessions_busy]";
const BUSY_POLL_MS: u64 = 2000;
const BUSY_WAIT_BUDGET_MS: u128 = 300_000; // stop queueing after 5 minutes

/// Text prefill batch. An IMAGE is never chunked - it goes to the host whole,
/// which is what lets the host decide the batch geometry its projector needs.
const PREFILL_CHUNK: usize = 128;

// ----------------------------------------------------------------- session --

fn nn_err(stage: &str, e: bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

/// Diagnose a failed load_by_name(). A NotFound is NOT one condition - the
/// graph registry seals at process start, so it splits three ways, and telling
/// an operator the wrong one is worse than saying nothing.
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
            "[volume_not_attached] {base} - the \"{vol}\" volume is not attached; deploy with \
             {{\"volumes\":[\"{vol}\"]}} in the config, or tick it in the console's volume picker"
        );
    }
    match preloaded_graphs() {
        Some(pre) if pre.iter().any(|p| p == vol) => format!(
            "[host_load_failed] {base} - the host tried to load \"{vol}\" when this deployment \
             started and FAILED (the deployment log has the reason); if this persists, the \
             deployment's share cannot hold the model"
        ),
        Some(_) => format!(
            "[model_not_loaded] {base} - \"{vol}\" is attached but was not loaded when this \
             deployment started (the volume finished mounting later, or it exceeds the share's \
             VRAM budget); the platform restarts the deployment to load it - retry shortly"
        ),
        None => format!(
            "{base} (is the \"{vol}\" volume attached, and does it carry a GGUF? this app needs \
             a GPU-share deployment - the host preloads the model)"
        ),
    }
}

/// What the host says this session can do. The list has only grown, so an older
/// host returns fewer values and each missing one reads as "no".
pub struct Caps {
    /// the sequence handle another session names to branch from this one.
    /// Unused here (no speculative decoding), but reading it is how the
    /// capability probe itself works.
    #[allow(dead_code)]
    pub seq: i32,
    #[allow(dead_code)]
    pub recurrent: bool,
    #[allow(dead_code)]
    pub mtp: bool,
    /// the volume carries a projector AND this node can drive it. The one cap
    /// this app cannot work without.
    pub vision: bool,
}

/// A live inference session. The KV cache lives HOST-side inside the execution
/// context; the guest feeds token ids and image bytes and reads one logits row.
pub struct Session {
    ctx: GraphExecutionContext,
}

impl Session {
    /// Open a session, QUEUEING while the host's session gate says every slot
    /// is taken. `status` narrates the wait (and doubles as the keepalive that
    /// keeps a streaming client from timing out); returning false from it means
    /// the client is gone.
    pub fn open(cfg: &AppConfig, status: &dyn Fn(&str) -> bool) -> Result<Session, String> {
        let t0 = now_ms();
        loop {
            match Session::open_once(cfg) {
                Ok(s) => return Ok(s),
                Err(e) if e.contains(BUSY_MARKER) => {
                    if now_ms() - t0 > BUSY_WAIT_BUDGET_MS {
                        return Err(format!(
                            "[sessions_busy] every inference session stayed busy for {}s - this \
                             deployment is at its concurrent capacity right now; try again in a \
                             little while",
                            BUSY_WAIT_BUDGET_MS / 1000
                        ));
                    }
                    if !status(&format!(
                        "all inference sessions are busy ({}s) - waiting for a free slot",
                        (now_ms() - t0) / 1000
                    )) {
                        return Err("client disconnected".into());
                    }
                    sleep_ms(BUSY_POLL_MS);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn open_once(cfg: &AppConfig) -> Result<Session, String> {
        let graph = load_by_name(&cfg.model_volume).map_err(|e| ggml_load_err(cfg, e))?;
        let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
        Ok(Session { ctx })
    }

    /// This session's capabilities. Errors on hosts that predate the caps verb,
    /// which is itself the probe.
    pub fn caps(&mut self) -> Result<Caps, String> {
        let outs = self
            .ctx
            .compute(vec![("caps".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes()))])
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

    /// Feed `ids`; with `want_logits`, return the LAST token's logits row.
    pub fn feed(&mut self, cfg: &AppConfig, ids: &[u32], want_logits: bool) -> Result<Vec<f32>, String> {
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = self
            .ctx
            .compute(vec![(
                "tokens".to_string(),
                Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes),
            )])
            .map_err(|e| nn_err("compute", e))?;
        if !want_logits {
            return Ok(Vec::new());
        }
        let logits = outs
            .iter()
            .find(|(n, _)| n == "logits")
            .ok_or("ggml backend returned no \"logits\" output")?;
        let data = logits.1.data();
        if data.len() != cfg.vocab * 4 {
            return Err(format!(
                "ggml logits are {} bytes, config vocab says {} - wrong model_volume for this \
                 config?",
                data.len(),
                cfg.vocab * 4
            ));
        }
        Ok(data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }

    /// Hand ONE image to the host, which encodes it and splices the result into
    /// this sequence at the current position. Returns the POSITIONS it consumed
    /// - not a token count, and not something the guest could derive: a
    /// dynamic-resolution model prices an image by its own grid, and M-RoPE
    /// numbers image positions differently again. Measured on Qwen3-VL, a
    /// picture reported as 22 "tokens" occupied ~240 KV cells; a guest that
    /// assumed one position per token would corrupt every position after it.
    pub fn feed_image(&mut self, bytes: &[u8]) -> Result<usize, String> {
        let outs = self
            .ctx
            .compute(vec![(
                "image".to_string(),
                Tensor::new(&[bytes.len() as u32], TensorType::U8, bytes),
            )])
            .map_err(|e| {
                let e = nn_err("image", e);
                // A host that KNOWS the verb tags what went wrong with one of
                // its own markers. Anything else came from a host that does not
                // know the verb at all - and what such a host says ("missing
                // \"tokens\" input") is not a sentence anyone can act on, so
                // name the real condition instead.
                const KNOWN: &[&str] =
                    &["[image_undecodable]", "[image_too_wide]", "[vision_unavailable]", "[kv_pool_full]"];
                if KNOWN.iter().any(|m| e.contains(m)) {
                    e
                } else {
                    format!(
                        "[vision_unsupported] this deployment's node cannot read images: its \
                         llama.cpp toolchain predates vision support, so the model never got the \
                         picture (host said: {e})"
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
}

// ------------------------------------------------------------------ prompt --

/// One turn of the request. `images` are raw file bytes, already decoded from
/// whatever transport spelling the caller used.
#[derive(Clone)]
pub struct Turn {
    pub role: String,
    pub text: String,
    pub images: Vec<Vec<u8>>,
}

/// One run of a prompt: either tokens, or a picture the HOST has to encode.
pub enum PromptPart {
    Text(Vec<u32>),
    /// raw image file bytes, handed to the host's "image" verb verbatim
    Image(Vec<u8>),
}

/// A prompt ready to feed: its parts in order, plus the flat token stream for
/// everything that IS text (the repetition window and the token counts read
/// that side; an image contributes no token ids to look back at).
pub struct Prompt {
    pub parts: Vec<PromptPart>,
    pub text_ids: Vec<u32>,
    pub images: usize,
}

/// Remove media marks from text that came from outside.
fn strip_marks(s: &str) -> String {
    if s.contains(config::MEDIA_MARK) {
        return s.replace(config::MEDIA_MARK, "");
    }
    s.to_string()
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
fn split_rendered(tok: &Tokenizer, rendered: &str, images: Vec<Vec<u8>>) -> Result<Prompt, String> {
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
            let enc = tok.encode(*chunk, i == 0).map_err(|e| format!("tokenize: {e}"))?;
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

/// Render + tokenize the request; drops oldest turns until it fits.
///
/// IMAGES: an attachment leaves a MEDIA_MARK in its turn's text, the template
/// renders the conversation as usual, and the result is split back apart on
/// those marks. So the model receives its own chat format exactly as it was
/// trained on it, with the pictures sitting where the marks were, and this
/// function needs to know nothing about how any particular VLM wraps an image
/// (the host's projector adds whatever marker tokens the model expects).
///
/// PICTURES LEAD THE TURN, then the words about them. That order is not
/// cosmetic: every instruction-tuned VLM is trained with the image first, and
/// a question that arrives before its picture measurably degrades the answer.
pub fn build_prompt(
    cfg: &AppConfig,
    tok: &Tokenizer,
    system: Option<&str>,
    turns: &[Turn],
) -> Result<(Prompt, Vec<String>), String> {
    let system = system.map(strip_marks).unwrap_or_else(|| cfg.system_prompt.clone());
    let mut msgs: Vec<(String, String)> = Vec::new();
    let mut turn_images: Vec<&[Vec<u8>]> = Vec::new();
    for t in turns.iter().filter(|t| t.role == "user" || t.role == "assistant") {
        let mut content = strip_marks(&t.text);
        if !t.images.is_empty() {
            let marks = config::MEDIA_MARK.repeat(t.images.len());
            content = if content.trim().is_empty() {
                // a bare image with no instruction is out of distribution for a
                // chat-tuned VLM, and the config says what a caller who asked
                // nothing meant
                format!("{marks}\n{}", cfg.default_question)
            } else {
                format!("{marks}\n{content}")
            };
        }
        msgs.push((t.role.clone(), content));
        turn_images.push(&t.images);
    }
    if msgs.is_empty() {
        return Err("no user turns in this request".into());
    }
    loop {
        let rendered = config::render_template(&cfg.template, &system, &msgs)?;
        let images: usize = turn_images.iter().map(|im| im.len()).sum();
        let total = tokens_of(tok, &rendered.prompt)? + images * cfg.image_tokens;
        if total <= cfg.max_prompt_tokens || msgs.len() <= 1 {
            if total > cfg.max_prompt_tokens {
                return Err(format!(
                    "[prompt_too_long] this request is {total} tokens (limit {}){}",
                    cfg.max_prompt_tokens,
                    if images > 0 {
                        format!(
                            " - {images} image(s) are budgeted at {} tokens each, so fewer or \
                             smaller pictures is the lever here",
                            cfg.image_tokens
                        )
                    } else {
                        String::new()
                    }
                ));
            }
            // The picture bytes are cloned ONCE, for the render that actually
            // fits: a trim round about to be thrown away has no business
            // copying several megabytes to find that out.
            let bytes: Vec<Vec<u8>> = turn_images.iter().flat_map(|im| im.iter().cloned()).collect();
            let prompt = split_rendered(tok, &rendered.prompt, bytes)?;
            return Ok((prompt, rendered.stop_strings));
        }
        msgs.remove(0); // drop the oldest turn and retry
        turn_images.remove(0); // ...and the pictures that were part of it
    }
}

/// Vet a request's attachments against the model and the deployment's limits,
/// BEFORE any of the expensive machinery starts. Returns how many images the
/// request carries.
///
/// Each failure names the thing to change, because each is a different mistake:
/// no picture at all, too many, one too large, or a volume that cannot see.
pub fn check_images(cfg: &AppConfig, turns: &[Turn]) -> Result<usize, String> {
    let n: usize = turns.iter().map(|t| t.images.len()).sum();
    if n == 0 {
        if cfg.require_image {
            return Err(
                "[no_image] this request carries no image, and this deployment serves vision \
                 only - a picture is what it is for. Attach one as an OpenAI content part \
                 ({\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,...\"}}) \
                 or as `image`/`images` on /v1/vision. Set \"require_image\": false in the \
                 deployment config to allow text-only turns."
                    .into(),
            );
        }
        return Ok(0);
    }
    if n > cfg.max_images {
        return Err(format!(
            "[too_many_images] {n} images in one request; this deployment allows {} (each costs \
             a vision-encoder pass and about {} tokens of the context window)",
            cfg.max_images, cfg.image_tokens
        ));
    }
    if let Some(big) =
        turns.iter().flat_map(|t| t.images.iter()).find(|b| b.len() > cfg.max_image_bytes)
    {
        return Err(format!(
            "[image_too_large] an image is {} KB; the limit is {} KB - resize it before sending \
             (the playground does this in the browser)",
            big.len() / 1024,
            cfg.max_image_bytes / 1024
        ));
    }
    Ok(n)
}

// -------------------------------------------------------------- generation --

pub struct GenParams {
    pub max_new: usize,
    pub sample: SampleParams,
    pub stop_strings: Vec<String>,
    /// identical consecutive token blocks that end a degenerate answer (0 = off)
    pub loop_reps: usize,
}

pub struct GenStats {
    pub prompt_tokens: usize,
    pub tokens: usize,
    pub load_ms: u128,
    pub prefill_ms: u128,
    pub decode_ms: u128,
    pub finish_reason: &'static str,
    pub text: String,
    /// images the host encoded into this prompt, and the positions they
    /// actually cost. The positions are the HOST's own figure, not the config's
    /// budget estimate, so a deployment can see what its window really went on.
    pub images: usize,
    pub image_pos: usize,
}

/// The incremental text pipeline: push sampled tokens one at a time; it
/// detokenizes the sequence, holds back the longest stop string so one is never
/// partially emitted, sends stable deltas, and reports a stop-string hit.
enum Pushed {
    More,
    Stopped,
    Gone,
}

struct TextOut<'a> {
    tok: &'a Tokenizer,
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
            tok,
            emit,
            stops,
            holdback: stops.iter().map(|s| s.len()).max().unwrap_or(0),
            generated: Vec::new(),
            emitted: 0,
            text: String::new(),
        }
    }

    fn push(&mut self, next: u32) -> Pushed {
        self.generated.push(next);
        if let Ok(text) = self.tok.decode(&self.generated, true) {
            if let Some(pos) = self.stops.iter().filter_map(|s| text.find(s.as_str())).min() {
                self.text = text[..pos].to_string();
                if pos > self.emitted {
                    if !(self.emit)(&text[self.emitted..pos]) {
                        return Pushed::Gone;
                    }
                    self.emitted = pos;
                }
                return Pushed::Stopped;
            }
            let visible = text.len().saturating_sub(self.holdback);
            if !text.ends_with('\u{FFFD}') && visible > self.emitted {
                if let Some(delta) = text.get(self.emitted..visible) {
                    if !(self.emit)(delta) {
                        return Pushed::Gone;
                    }
                    self.emitted = visible;
                }
            }
            self.text = text;
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

    /// the repetition-penalty window: the freshest `w` sampled tokens, falling
    /// back to the prompt tail before anything is generated
    fn recent(&self, prompt_ids: &[u32], w: usize) -> Vec<u32> {
        if self.generated.is_empty() {
            prompt_ids[prompt_ids.len().saturating_sub(w)..].to_vec()
        } else {
            self.generated[self.generated.len().saturating_sub(w)..].to_vec()
        }
    }
}

/// Stops an answer that has collapsed into a loop. Dense images are exactly
/// the input that provokes one - a table, a legend, a list of labels - and
/// sampling settings reduce the odds without ever promising anything, so the
/// decoder needs a hard stop and not just better odds.
///
/// Detection is exact-block, not fuzzy: the tail must be one identical run of
/// tokens repeated N times. That misses a drifting near-repeat, but it also
/// cannot fire on a transcription that legitimately repeats a row prefix - and
/// a false stop truncates someone's real answer, which is worse than a loop
/// that runs a little longer before tripping.
struct LoopGuard {
    reps: usize,
    max_period: usize,
}

impl LoopGuard {
    fn new(reps: usize) -> LoopGuard {
        LoopGuard { reps, max_period: 64 }
    }

    /// Short blocks need more evidence: a couple of repeated newlines or a run
    /// of dashes in a table is ordinary text, while the same twelve tokens four
    /// times over is not.
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
            let span = period * self.required(period);
            // NOT `break`: required() demands extra repeats of short blocks, so
            // span does not grow monotonically with period.
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

/// Run one answer. `emit` receives text deltas as they stabilize, `status`
/// receives progress lines; both return false when the client is gone. Status
/// events double as keepalive bytes through the two long silences - a cold
/// session init, and the vision encoder's pass over a large picture.
pub fn generate(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt: &Prompt,
    p: &GenParams,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let prompt_ids: &[u32] = &prompt.text_ids;
    if !status(
        "opening an inference session - the first request after a node boot initializes the \
         model and can take a while",
    ) {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    let mut sess = Session::open(cfg, status)?;
    let load_ms = now_ms() - t0;

    // The config said this volume can see; ask the HOST, which is the half that
    // actually has to (the projector in the volume, and a node new enough to
    // drive it). Doing it here, before the prefill, means a deployment whose
    // node predates vision fails in a sentence instead of after a minute of
    // encode work. A host too old to answer `caps` at all is not judged: the
    // image feed itself will say so, and that path has the better message.
    if prompt.images > 0 {
        if let Ok(caps) = sess.caps() {
            if !caps.vision {
                return Err(format!(
                    "[no_vision] the \"{}\" volume on this node cannot read images: the node \
                     reports no projector (is an *mmproj*.gguf in the volume, and is this \
                     node's llama.cpp toolchain new enough?)",
                    cfg.model_volume
                ));
            }
        }
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

    // -- prefill. Text goes in chunks so no single logits tensor gets huge; an
    // image goes WHOLE, to the host, which encodes it and splices the result
    // into the sequence at the position the text left off.
    let t1 = now_ms();
    let mut logits = Vec::new();
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
                    "looking at the image ({} KB) - the vision encoder runs on the same share as \
                     the model",
                    bytes.len() / 1024
                )) {
                    return Err("client disconnected".into());
                }
                image_pos += sess.feed_image(bytes)?;
            }
        }
    }
    if logits.is_empty() {
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
    let loop_guard = LoopGuard::new(p.loop_reps);
    let mut finish: &'static str = "stop";
    loop {
        let recent = out.recent(prompt_ids, p.sample.rep_window);
        let next = pick_token(&mut logits, &recent, &p.sample, &mut rng);
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
                    prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(),
                    load_ms,
                    prefill_ms,
                    decode_ms,
                    finish_reason: "stop",
                    text: out.text,
                    images: prompt.images,
                    image_pos,
                });
            }
            Pushed::Gone => break,
        }
        if loop_guard.tripped(&out.generated) {
            finish = "repetition";
            break;
        }
        logits = sess.feed(cfg, &[next], true)?;
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(),
        load_ms,
        prefill_ms,
        decode_ms,
        finish_reason: finish,
        text: out.text,
        images: prompt.images,
        image_pos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AppConfig {
        config::from_value(serde_json::json!({
            "name": "t", "n_layers": 4, "n_kv_heads": 2, "head_dim": 8, "vocab": 32,
            "eos": [1], "template": "chatml", "system_prompt": "sys",
            "max_prompt_tokens": 4096, "default_max_new": 64, "max_new_cap": 128,
            "model_volume": "vol", "image_tokens": 100, "max_images": 2,
            "max_image_bytes": 1024
        }))
        .unwrap()
    }

    fn turn(text: &str, images: usize) -> Turn {
        Turn {
            role: "user".into(),
            text: text.into(),
            images: (0..images).map(|_| vec![0u8; 8]).collect(),
        }
    }

    #[test]
    fn a_projector_is_not_mistaken_for_a_model() {
        assert!(is_mmproj(Path::new("/models/v/mmproj-Qwen3VL-8B-Instruct-F16.gguf")));
        assert!(is_mmproj(Path::new("/models/v/MMPROJ-F16.GGUF")));
        assert!(!is_mmproj(Path::new("/models/v/Qwen3VL-8B-Instruct-Q8_0.gguf")));
        assert!(!is_mmproj(Path::new("/models/v/model.gguf")));
    }

    #[test]
    fn a_request_with_no_picture_is_refused_by_default() {
        let e = check_images(&cfg(), &[turn("what is this", 0)]).unwrap_err();
        assert!(e.starts_with("[no_image]"), "{e}");
        // ...and allowed when the deployment says so
        let mut c = cfg();
        c.require_image = false;
        assert_eq!(check_images(&c, &[turn("hello", 0)]).unwrap(), 0);
    }

    #[test]
    fn the_image_limits_are_reported_separately() {
        let c = cfg();
        assert_eq!(check_images(&c, &[turn("q", 2)]).unwrap(), 2);
        assert!(check_images(&c, &[turn("q", 3)]).unwrap_err().starts_with("[too_many_images]"));
        let big = Turn { role: "user".into(), text: "q".into(), images: vec![vec![0u8; 2048]] };
        assert!(check_images(&c, &[big]).unwrap_err().starts_with("[image_too_large]"));
    }

    #[test]
    fn a_loop_is_caught_and_a_repeating_row_prefix_is_not() {
        let g = LoopGuard::new(4);
        // twelve tokens, four times over
        let unit: Vec<u32> = (0..12).collect();
        let mut looped = Vec::new();
        for _ in 0..4 {
            looped.extend_from_slice(&unit);
        }
        assert!(g.tripped(&looped));
        // three times is not yet evidence
        assert!(!g.tripped(&looped[..unit.len() * 3]));
        // a single token needs far more repeats before it counts as a loop
        assert!(!g.tripped(&vec![7u32; 12]));
        assert!(g.tripped(&vec![7u32; 24]));
        // off means off
        assert!(!LoopGuard::new(0).tripped(&looped));
    }

    #[test]
    fn a_forged_media_mark_cannot_claim_an_image_slot() {
        let crafted = format!("look{}here", config::MEDIA_MARK);
        assert_eq!(strip_marks(&crafted), "lookhere");
        // and the count that matters is the one build_prompt plants itself
        assert_eq!(
            strip_marks(&format!("{}{}", config::MEDIA_MARK, config::MEDIA_MARK)),
            ""
        );
    }

    #[test]
    fn kv_element_widths_match_the_quant_names() {
        assert_eq!(kv_elem_sixteenths("f16"), 32);
        assert_eq!(kv_elem_sixteenths("q8_0"), 17);
        assert_eq!(kv_elem_sixteenths(""), 32); // unknown reads as f16
    }
}
