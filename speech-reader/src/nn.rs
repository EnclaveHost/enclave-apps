//! The inference half: model volumes, the ggml session, the audio verb, and
//! one transcription.
//!
//! BACKEND: ggml (llama.cpp) only. Granite Speech is a conformer audio
//! encoder (the volume's *mmproj*.gguf - the SAME naming convention that
//! carries a vision projector, because to libmtmd they are the same kind of
//! thing) projecting into a dense granite LM with the speech adapters merged.
//! The audio file crosses to the host as FILE BYTES through the "audio" verb,
//! the exact mirror of image-reader's "image" verb: decoding, resampling, the
//! encoder, the projector and the model's marker tokens all live host-side,
//! and the guest learns only how many POSITIONS the sequence advanced.
//!
//! The "audio" verb is a small host addition specced in PLATFORM.md. On a
//! node that predates it, this app fails with [audio_unsupported] and the
//! sentence that names the fix - never with silence.

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
         GUEST tokenizes the prompt",
        cfg.model_volume,
        rels.join(", ")
    ))
}

pub fn is_mmproj(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("mmproj"))
        .unwrap_or(false)
}

/// The audio encoder's size in the model volume, if it carries one. `None` is
/// the most useful fact this app can report about a volume: a speech volume
/// without its encoder is a language model that has never heard anything.
pub fn mmproj_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false) && is_mmproj(p))
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
        .max()
}

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

/// Locate the weights WITHOUT reading them, mirroring the host's pick, with
/// the projector taken out first (same as image-reader).
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

pub struct ModelEntry {
    pub volume: String,
    pub bytes: u64,
    /// the audio encoder's size, or None when the volume cannot hear
    pub mmproj: Option<u64>,
    pub cfg: AppConfig,
}

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

const WORKING_SET: u64 = 3 << 29; // 1.5 GiB

/// VRAM to SERVE beyond resident weights: KV at the NODE's window, plus the
/// AUDIO ENCODER, priced here for the same reason image-reader prices its
/// projector: it loads LAZILY, on the first clip this deployment hears, and a
/// share that fits the LM alone would serve /health fine and then abort the
/// whole tenant on the first recording (CUDA OOM inside compute() calls
/// ggml_abort; no error reaches any guest).
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
    // encoder + its workspace; a volume with no encoder still gets the
    // allowance, because the load attempt must not be an OOM
    let hearing = mmproj_size(cfg).unwrap_or(1 << 30) + (1 << 29);
    if pooled_backend() {
        return (kv + hearing, WORKING_SET);
    }
    let sessions = std::env::var("ENCLAVE_GGML_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    (kv * sessions + hearing, WORKING_SET * sessions)
}

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
                 context window + {:.1} GB audio encoder and working set) but {:.1} GB of the \
                 {:.1} GB VRAM budget remains - redeploy with a larger GPU share",
                gb(need),
                gb(e.bytes),
                gb(kv.saturating_sub(e.mmproj.unwrap_or(0))),
                gb(e.mmproj.unwrap_or(0) + ws),
                gb(budget.saturating_sub(claimed)),
                gb(budget)
            ),
        );
    }
    out
}

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
    // prefer a model that fits AND can hear
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

fn sleep_ms(ms: u64) {
    let p = bindings::wasi::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000);
    bindings::wasi::io::poll::poll(&[&p]);
}

const BUSY_MARKER: &str = "[sessions_busy]";
const BUSY_POLL_MS: u64 = 2000;
const BUSY_WAIT_BUDGET_MS: u128 = 300_000;

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
             started and FAILED (the deployment log has the reason)"
        ),
        Some(_) => format!(
            "[model_not_loaded] {base} - \"{vol}\" is attached but was not loaded when this \
             deployment started; the platform restarts the deployment to load it - retry shortly"
        ),
        None => format!(
            "{base} (is the \"{vol}\" volume attached, and does it carry a GGUF? this app needs \
             a GPU-share deployment - the host preloads the model)"
        ),
    }
}

/// What the host says this session can do. The caps list has only grown;
/// missing slots read as "no". Slot 7 is the audio bit PLATFORM.md specs.
pub struct Caps {
    pub audio: Option<bool>, // None = host too old to say
}

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

    /// The audio capability, from the host's own mouth. Slot 7 of the caps
    /// verb (PLATFORM.md): present and nonzero = this node's shim exports the
    /// audio verb AND this volume's projector can hear.
    pub fn caps(&mut self) -> Result<Caps, String> {
        let outs = self
            .ctx
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
        let v = |i: usize| {
            i32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]])
        };
        Ok(Caps { audio: (data.len() >= 32).then(|| v(7) != 0) })
    }

    /// Feed `ids`; with `want_logits`, return the LAST token's logits row.
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
                 config?",
                data.len(),
                cfg.vocab * 4
            ));
        }
        Ok(data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }

    /// Hand ONE audio file to the host, which decodes it (wav/mp3/flac),
    /// resamples, runs the conformer encoder + projector, and splices the
    /// result into this sequence. Returns the POSITIONS consumed - the host's
    /// figure, not a guest guess.
    pub fn feed_audio(&mut self, bytes: &[u8]) -> Result<usize, String> {
        let outs = self
            .ctx
            .compute(vec![(
                "audio".to_string(),
                Tensor::new(&[bytes.len() as u32], TensorType::U8, bytes),
            )])
            .map_err(|e| {
                let e = nn_err("audio", e);
                // A host that KNOWS the verb tags failures with its own
                // markers; anything else is a host that never heard of it,
                // and ITS words ("missing \"tokens\" input") are not a
                // sentence anyone can act on.
                const KNOWN: &[&str] =
                    &["[audio_undecodable]", "[audio_unavailable]", "[kv_pool_full]", "[audio_too_long]"];
                if KNOWN.iter().any(|m| e.contains(m)) {
                    e
                } else {
                    format!(
                        "[audio_unsupported] this deployment's node cannot hear: its shim \
                         predates the audio verb (PLATFORM.md in this app is the spec for \
                         adding it), so the recording never reached the model (host said: {e})"
                    )
                }
            })?;
        let n = outs
            .iter()
            .find(|(n, _)| n == "audio_pos")
            .ok_or("[audio_unsupported] host returned no \"audio_pos\" output")?;
        let data = n.1.data();
        if data.len() < 4 {
            return Err("host returned a malformed audio_pos".into());
        }
        let pos = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if pos <= 0 {
            return Err("the model consumed no positions for this audio".into());
        }
        Ok(pos as usize)
    }
}

// -------------------------------------------------------------- generation --

/// One piece of audio to transcribe as its own episode.
pub struct Clip {
    pub bytes: Vec<u8>,
    /// known for WAV (admission + segment timestamps); None for mp3/flac
    pub seconds: Option<f32>,
}

pub struct TranscribeParams {
    /// the task instruction, placed after the audio in the turn
    pub instruction: String,
    pub clips: Vec<Clip>,
    pub sample: SampleParams,
    pub max_new: usize,
    pub loop_reps: usize,
}

pub struct Segment {
    /// start offset within the request, seconds - exact chunk arithmetic for
    /// WAV, None when the source durations were unknowable
    pub start: Option<f32>,
    pub seconds: Option<f32>,
    pub text: String,
}

pub struct TranscribeStats {
    pub prompt_tokens: usize,
    pub audio_pos: usize,
    pub tokens: usize,
    pub load_ms: u128,
    pub prefill_ms: u128,
    pub decode_ms: u128,
    pub finish_reason: &'static str,
    pub segments: Vec<Segment>,
    pub text: String,
}

/// The incremental text pipeline, from image-reader: holds back the longest
/// stop string so one is never partially emitted, sends stable deltas.
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

    fn flush(&mut self) {
        if self.text.len() > self.emitted {
            if let Some(delta) = self.text.get(self.emitted..) {
                let _ = (self.emit)(delta);
            }
        }
    }
}

/// Stops a transcript that has collapsed into a loop (verbatim from
/// image-reader; silence and hum are ASR's dense tables). Exact-block only:
/// a false stop truncates someone's real words, which is worse than a loop
/// running a little longer.
struct LoopGuard {
    reps: usize,
    max_period: usize,
}

impl LoopGuard {
    fn new(reps: usize) -> LoopGuard {
        LoopGuard { reps, max_period: 64 }
    }

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

/// Transcribe every clip in order, streaming text deltas through `emit` and
/// progress through `status` (both return false when the client is gone).
/// Each clip is one episode in a fresh session: Granite Speech is trained on
/// single (audio, instruction) turns at 4096 positions, and a fresh window
/// per episode is what keeps a long recording from walking off the model's
/// trained context.
pub fn transcribe(
    cfg: &AppConfig,
    tok: &Tokenizer,
    p: &TranscribeParams,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<TranscribeStats, String> {
    let mut st = TranscribeStats {
        prompt_tokens: 0,
        audio_pos: 0,
        tokens: 0,
        load_ms: 0,
        prefill_ms: 0,
        decode_ms: 0,
        finish_reason: "stop",
        segments: Vec::new(),
        text: String::new(),
    };
    let prefix_ids = tok
        .encode(cfg.prompt_prefix.as_str(), false)
        .map_err(|e| format!("tokenize: {e}"))?
        .get_ids()
        .to_vec();
    let suffix_text = format!("{}{}", p.instruction, cfg.prompt_suffix);
    let suffix_ids = tok
        .encode(suffix_text.as_str(), false)
        .map_err(|e| format!("tokenize: {e}"))?
        .get_ids()
        .to_vec();

    let mut clock = 0.0f32;
    for (ci, clip) in p.clips.iter().enumerate() {
        if !status(&format!(
            "transcribing part {}/{}{}",
            ci + 1,
            p.clips.len(),
            clip.seconds.map(|s| format!(" ({s:.0}s)")).unwrap_or_default()
        )) {
            return Err("client disconnected".into());
        }
        let t0 = now_ms();
        let mut sess = Session::open(cfg, status)?;
        st.load_ms += now_ms() - t0;

        // caps says whether this node can hear BEFORE the encoder runs; a
        // host too old to answer caps at all is not judged here - the audio
        // feed itself has the better message.
        if ci == 0 {
            if let Ok(caps) = sess.caps() {
                if caps.audio == Some(false) {
                    return Err(format!(
                        "[audio_unsupported] the \"{}\" volume on this node cannot hear: the \
                         host reports no audio support for it (is the *mmproj*.gguf audio \
                         encoder in the volume, and does this node's shim carry the audio \
                         verb? PLATFORM.md is the spec)",
                        cfg.model_volume
                    ));
                }
            }
        }

        // -- prefill: prefix, the audio, then instruction + assistant cue
        let t1 = now_ms();
        sess.feed(cfg, &prefix_ids, false)?;
        let audio_pos = sess.feed_audio(&clip.bytes)?;
        st.audio_pos += audio_pos;
        let mut logits = Vec::new();
        let mut done = 0usize;
        while done < suffix_ids.len() {
            let end = (done + PREFILL_CHUNK).min(suffix_ids.len());
            let last = end == suffix_ids.len();
            let l = sess.feed(cfg, &suffix_ids[done..end], last)?;
            if last {
                logits = l;
            }
            done = end;
        }
        st.prompt_tokens += prefix_ids.len() + suffix_ids.len();
        st.prefill_ms += now_ms() - t1;

        // the episode's remaining position budget bounds its transcript
        let consumed = prefix_ids.len() + audio_pos + suffix_ids.len();
        let max_new = p.max_new.min(cfg.max_positions.saturating_sub(consumed + 8));
        if max_new == 0 {
            return Err(format!(
                "[audio_too_long] this clip consumed {audio_pos} positions and the prompt \
                 {} more - nothing is left of the {}-position episode budget for the \
                 transcript itself. Shorter chunks (chunk_seconds) are the lever.",
                consumed - audio_pos,
                cfg.max_positions
            ));
        }

        // -- decode
        let t2 = now_ms();
        let mut rng = Rng::new(now_ms() as u64 ^ (audio_pos as u64) << 17);
        let seg_emit = |d: &str| emit(d);
        let mut out = TextOut::new(tok, &seg_emit, &cfg.stop_strings);
        let loop_guard = LoopGuard::new(p.loop_reps);
        loop {
            let recent = out.generated[out.generated.len().saturating_sub(p.sample.rep_window)..]
                .to_vec();
            let next = pick_token(&mut logits, &recent, &p.sample, &mut rng);
            if cfg.eos.contains(&next) {
                break;
            }
            if out.generated.len() >= max_new {
                st.finish_reason = "length";
                break;
            }
            match out.push(next) {
                Pushed::More => {}
                Pushed::Stopped => break,
                Pushed::Gone => return Err("client disconnected".into()),
            }
            if loop_guard.tripped(&out.generated) {
                st.finish_reason = "repetition";
                break;
            }
            logits = sess.feed(cfg, &[next], true)?;
        }
        out.flush();
        st.decode_ms += now_ms() - t2;
        st.tokens += out.generated.len();
        drop(sess);

        let text = out.text.trim().to_string();
        if !st.text.is_empty() && !text.is_empty() {
            st.text.push(' ');
        }
        st.text.push_str(&text);
        st.segments.push(Segment {
            start: clip.seconds.map(|_| clock),
            seconds: clip.seconds,
            text,
        });
        clock += clip.seconds.unwrap_or(0.0);
        // a clean boundary between episodes for the streaming reader
        if ci + 1 < p.clips.len() && !emit(" ") {
            return Err("client disconnected".into());
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_projector_is_not_mistaken_for_a_model() {
        assert!(is_mmproj(Path::new("/models/v/mmproj-model-f16.gguf")));
        assert!(!is_mmproj(Path::new("/models/v/granite-speech-4.1-2b-Q8_0.gguf")));
    }

    #[test]
    fn a_loop_is_caught_and_short_blocks_need_more_evidence() {
        let g = LoopGuard::new(4);
        let unit: Vec<u32> = (0..12).collect();
        let mut looped = Vec::new();
        for _ in 0..4 {
            looped.extend_from_slice(&unit);
        }
        assert!(g.tripped(&looped));
        assert!(!g.tripped(&looped[..unit.len() * 3]));
        assert!(!g.tripped(&vec![7u32; 12]));
        assert!(g.tripped(&vec![7u32; 24]));
        assert!(!LoopGuard::new(0).tripped(&looped));
    }

    #[test]
    fn kv_element_widths_match_the_quant_names() {
        assert_eq!(kv_elem_sixteenths("f16"), 32);
        assert_eq!(kv_elem_sixteenths("q8_0"), 17);
        assert_eq!(kv_elem_sixteenths(""), 32);
    }
}
