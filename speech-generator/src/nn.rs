//! The inference half: model volumes, the ggml session, and one spoken take.
//!
//! BACKEND: ggml (llama.cpp) only. Maya1 is an ordinary Llama-3.2-3B as far as
//! the host is concerned - the audio protocol lives entirely in which token
//! ids the GUEST samples (sampling.rs) and what it does with them afterwards
//! (snac.rs). The host hands back one dense logits row per feed; this app
//! never asks for the sparse top-k rows the sibling eyesoff-ai uses, because
//! slot-constrained sampling needs an arbitrary 4096-id window of the row and
//! a top-256 could miss it entirely.
//!
//! THE DECODE LOOP INTERLEAVES generation and codec work: every EMIT_STEP
//! completed frames, the frames that already have their full halo context are
//! SNAC-decoded and pushed to the client as PCM, so audio flows while the
//! model is still speaking and the response stream is never silent long
//! enough for the gateway to cut it (~180 s after the last byte).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tokenizers::Tokenizer;

use crate::bindings;
use crate::bindings::wasi::nn::graph::load_by_name;
use crate::bindings::wasi::nn::inference::GraphExecutionContext;
use crate::bindings::wasi::nn::tensor::{Tensor, TensorType};
use crate::config::{self, AppConfig};
use crate::maya;
use crate::sampling::{pick_audio_token, Rng, SampleParams};
use crate::snac;

// ------------------------------------------------------------ model volumes --

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
/// tenant boot (ENCLAVE_NN_PRELOADS); see image-reader for the three-way
/// NotFound diagnosis this powers.
pub fn preloaded_graphs() -> Option<Vec<String>> {
    std::env::var("ENCLAVE_NN_PRELOADS").ok().map(|v| {
        v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
    })
}

fn volume_path(root: &PathBuf, rel: &str) -> PathBuf {
    if rel.starts_with('/') {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    }
}

fn volume_root(cfg: &AppConfig) -> Result<PathBuf, String> {
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
    Ok(root)
}

pub fn read_tokenizer(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    let root = volume_root(cfg)?;
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
         GUEST tokenizes the prompt",
        cfg.model_volume,
        rels.join(", ")
    ))
}

/// The SNAC decoder weights (tools/export_snac.py's container). This is the
/// file that makes a volume a SPEECH volume; without it the model could only
/// ever produce token ids nobody can hear.
pub fn read_snac(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    let root = volume_root(cfg)?;
    let p = volume_path(&root, &cfg.snac_file);
    if !p.is_file() {
        return Err(format!(
            "no {} in volume '{}' - a speech volume carries the SNAC decoder next to the \
             weights (fetch-model.sh builds it; tools/export_snac.py is the recipe)",
            cfg.snac_file, cfg.model_volume
        ));
    }
    std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display()))
}

pub fn snac_present(cfg: &AppConfig) -> bool {
    volume_root(cfg)
        .map(|r| volume_path(&r, &cfg.snac_file).is_file())
        .unwrap_or(false)
}

/// Split-GGUF family detection, verbatim from the sibling apps.
fn split_family(path: &std::path::Path) -> Option<Vec<PathBuf>> {
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

/// Locate the weights WITHOUT reading them - the host's own pick, mirrored
/// (model.gguf, else the single gguf, else one complete split family).
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

pub struct ModelEntry {
    pub volume: String,
    pub bytes: u64,
    /// the volume carries its SNAC decoder - without it the model is mute,
    /// which is this app's analogue of image-reader's missing projector
    pub snac: bool,
    pub cfg: AppConfig,
}

/// The servable models: attached volumes the catalog describes, largest first
/// (index 0 = default). A volume without snac_decoder.bin is listed but
/// flagged, so /health can say what is wrong with it.
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
        let snac = snac_present(&cfg);
        out.push(ModelEntry { volume: vol, bytes, snac, cfg });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.volume.cmp(&b.volume)));
    out
}

pub fn vram_budget() -> Option<u64> {
    std::env::var("ENCLAVE_VRAM_BYTES").ok()?.trim().parse::<u64>().ok().filter(|b| *b > 0)
}

pub fn gpu_present() -> Option<bool> {
    if vram_budget().is_some() {
        return Some(true);
    }
    std::env::vars().any(|(k, _)| k.starts_with("ENCLAVE_")).then_some(false)
}

fn kv_elem_sixteenths(t: &str) -> u64 {
    match t {
        "f32" => 64,
        "q8_0" => 17,
        "q4_0" => 9,
        "q4_1" => 10,
        _ => 32, // f16 / bf16 / unknown
    }
}

fn pooled_backend() -> bool {
    std::env::var("ENCLAVE_GGML_POOLED")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

const WORKING_SET: u64 = 3 << 29; // 1.5 GiB: compute buffers, CUDA context

/// Estimated VRAM to SERVE beyond resident weights: the KV cache at the NODE's
/// context window plus the working set. No lazy projector here - the SNAC
/// decoder runs in the GUEST on CPU - so this is the plain llm sizing. The
/// guard matters for the same reason as ever: a CUDA OOM inside compute()
/// aborts the whole tenant with no error reaching the guest.
pub fn serve_cost(cfg: &AppConfig) -> (u64, u64) {
    let Some(n_ctx) = std::env::var("ENCLAVE_GGML_N_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    else {
        return (0, 0);
    };
    let tk = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").unwrap_or_default();
    let tv = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").unwrap_or_else(|_| tk.clone());
    let elems =
        cfg.kv_layers.unwrap_or(cfg.n_layers) as u64 * cfg.n_kv_heads as u64 * cfg.head_dim as u64;
    let kv = elems * n_ctx * (kv_elem_sixteenths(tk.trim()) + kv_elem_sixteenths(tv.trim())) / 16;
    if pooled_backend() {
        return (kv, WORKING_SET);
    }
    let sessions = std::env::var("ENCLAVE_GGML_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    (kv * sessions, WORKING_SET * sessions)
}

/// Which servable models CANNOT serve within the VRAM budget (see
/// image-reader: refusing here is load-bearing, an OOM aborts the tenant).
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
            format!(
                "needs ~{:.1} GB to serve ({:.1} GB weights + {:.1} GB KV cache at the node's \
                 context window + {:.1} GB working set) but {:.1} GB of the {:.1} GB VRAM \
                 budget remains - redeploy with a larger GPU share",
                gb(need),
                gb(e.bytes),
                gb(kv),
                gb(ws),
                gb(budget.saturating_sub(claimed)),
                gb(budget)
            ),
        );
    }
    out
}

/// The AppConfig serving one request; unknown names fall back to the default
/// (SDKs always send a model string), known-but-unfit names are refused.
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
    // prefer a model that fits AND can be heard
    if let Some(e) = entries.iter().find(|e| !unfit.contains_key(&e.volume) && e.snac) {
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

fn sleep_ms(ms: u64) {
    let p = bindings::wasi::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000);
    bindings::wasi::io::poll::poll(&[&p]);
}

const BUSY_MARKER: &str = "[sessions_busy]";
const BUSY_POLL_MS: u64 = 2000;
/// Stop queueing for a session after this long. Shorter than image-reader's
/// 5 minutes DELIBERATELY: this app's response is a WAV stream whose keepalive
/// is a trickle of silence, and two minutes of silence before the first word
/// is where a listener gives up anyway.
const BUSY_WAIT_BUDGET_MS: u128 = 120_000;

const PREFILL_CHUNK: usize = 128;

// ----------------------------------------------------------------- session --

fn nn_err(stage: &str, e: bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

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

/// A live inference session. KV lives host-side; the guest feeds token ids and
/// reads one dense logits row.
pub struct Session {
    ctx: GraphExecutionContext,
}

impl Session {
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

    /// Feed `ids`; with `want_logits`, return the LAST token's dense row.
    pub fn feed(
        &mut self,
        cfg: &AppConfig,
        ids: &[u32],
        want_logits: bool,
    ) -> Result<Vec<f32>, String> {
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
                 config? (a speech config's vocab INCLUDES the 28,672 audio tokens)",
                data.len(),
                cfg.vocab * 4
            ));
        }
        Ok(data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }
}

// -------------------------------------------------------------- generation --

/// Frames withheld from streaming decode so every emitted frame has its full
/// halo context on both sides (24 latents = 6 frames; snac.rs).
const HALO_FRAMES: usize = 6;
/// Streaming decode granularity: ~1 s of audio per SNAC pass.
const EMIT_STEP: usize = 12;

pub struct SpeakParams {
    /// the resolved voice description
    pub desc: String,
    /// text chunks, each one generation episode (maya::chunk_text)
    pub chunks: Vec<String>,
    pub sample: SampleParams,
    pub max_new_per_chunk: usize,
    pub min_frames: usize,
    pub trim_warmup: usize,
    pub seed: u64,
}

pub struct SpeakStats {
    pub prompt_tokens: usize,
    pub audio_tokens: usize,
    pub frames: usize,
    pub samples: usize,
    pub load_ms: u128,
    pub prefill_ms: u128,
    pub decode_ms: u128,
    /// guest-side codec time, separated out because it is the part that runs
    /// on the wasm CPU rather than the GPU share
    pub snac_ms: u128,
    pub finish_reason: &'static str,
}

/// Speak every chunk in order, streaming PCM through `emit` as frames earn
/// their halo. Each chunk is a fresh session: a fresh context is what the
/// model saw in training, and dropping the session between chunks returns the
/// KV window to the host's pool while the guest is busy SNAC-decoding.
pub fn generate_speech(
    cfg: &AppConfig,
    tok: &Tokenizer,
    dec: &snac::Decoder,
    p: &SpeakParams,
    emit: &dyn Fn(&[f32]) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<SpeakStats, String> {
    let mut st = SpeakStats {
        prompt_tokens: 0,
        audio_tokens: 0,
        frames: 0,
        samples: 0,
        load_ms: 0,
        prefill_ms: 0,
        decode_ms: 0,
        snac_ms: 0,
        finish_reason: "stop",
    };
    let mut rng = Rng::new(p.seed);
    for (ci, chunk) in p.chunks.iter().enumerate() {
        if p.chunks.len() > 1
            && !status(&format!("speaking chunk {}/{}", ci + 1, p.chunks.len()))
        {
            return Err("client disconnected".into());
        }
        let text = maya::prompt_text(&p.desc, chunk);
        let enc = tok
            .encode(text.as_str(), false)
            .map_err(|e| format!("tokenize: {e}"))?;
        let ids = maya::prompt_ids(enc.get_ids());
        st.prompt_tokens += ids.len();

        let t0 = now_ms();
        let mut sess = Session::open(cfg, status)?;
        st.load_ms += now_ms() - t0;

        // -- prefill, logits only on the very last token
        let t1 = now_ms();
        let mut logits = Vec::new();
        let mut done = 0usize;
        while done < ids.len() {
            let end = (done + PREFILL_CHUNK).min(ids.len());
            let last = end == ids.len();
            let l = sess.feed(cfg, &ids[done..end], last)?;
            if last {
                logits = l;
            }
            done = end;
        }
        st.prefill_ms += now_ms() - t1;

        // -- decode: sample slot-constrained audio tokens, stream frames out
        let mut tokens: Vec<u32> = Vec::new();
        let mut emitted = 0usize; // frames already sent
        let seed = p.seed ^ (ci as u64).wrapping_mul(0x9e3779b97f4a7c15);
        loop {
            let slot = tokens.len() % maya::FRAME_TOKENS;
            let frames_done = tokens.len() / maya::FRAME_TOKENS;
            let allow_eos = slot == 0 && frames_done >= p.min_frames;
            let next = pick_audio_token(&logits, &tokens, allow_eos, &p.sample, &mut rng)?;
            if next == maya::CODE_EOS {
                break;
            }
            tokens.push(next);
            if tokens.len() >= p.max_new_per_chunk {
                st.finish_reason = "length";
                tokens.truncate(tokens.len() / maya::FRAME_TOKENS * maya::FRAME_TOKENS);
                break;
            }
            let frames_done = tokens.len() / maya::FRAME_TOKENS;
            if frames_done >= emitted + EMIT_STEP + HALO_FRAMES {
                let hi = frames_done - HALO_FRAMES;
                emit_frames(dec, &tokens, emitted, hi, seed, p, &mut st, emit)?;
                emitted = hi;
            }
            let t = now_ms();
            logits = sess.feed(cfg, &[next], true)?;
            st.decode_ms += now_ms() - t;
        }
        drop(sess);

        // -- flush the tail
        let frames_done = tokens.len() / maya::FRAME_TOKENS;
        if frames_done > emitted {
            emit_frames(dec, &tokens, emitted, frames_done, seed, p, &mut st, emit)?;
        }
        st.audio_tokens += tokens.len();
        st.frames += frames_done;
    }
    Ok(st)
}

/// Decode frames [lo, hi) of `tokens` and push the PCM. The warmup trim
/// applies once per chunk, at its head - the model card's 2048-sample
/// (one-frame) settle-in, which is model transient rather than codec halo.
#[allow(clippy::too_many_arguments)]
fn emit_frames(
    dec: &snac::Decoder,
    tokens: &[u32],
    lo: usize,
    hi: usize,
    seed: u64,
    p: &SpeakParams,
    st: &mut SpeakStats,
    emit: &dyn Fn(&[f32]) -> bool,
) -> Result<(), String> {
    let t = now_ms();
    let frames = tokens.len() / maya::FRAME_TOKENS;
    let codes = maya::unpack_frames(&tokens[..frames * maya::FRAME_TOKENS])?;
    let audio = dec.decode_frames(&codes, lo, hi, Some(seed))?;
    st.snac_ms += now_ms() - t;
    let skip = if lo == 0 { p.trim_warmup.min(audio.len()) } else { 0 };
    let audio = &audio[skip..];
    if audio.is_empty() {
        return Ok(());
    }
    st.samples += audio.len();
    if !emit(audio) {
        return Err("client disconnected".into());
    }
    Ok(())
}
