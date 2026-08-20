//! risc-box — run a real machine on the enclave's CPU, booted from OS images in
//! an S3 bucket, with its serial console bridged to your browser.
//!
//! Unlike golem (which ships QEMU-wasm to the browser and emulates in the
//! tab), risc-box emulates a full RISC-V machine **inside the enclave** — the
//! way QEMU installed on a server would. A vendored pure-Rust RISC-V system
//! emulator (takahirox/riscv-rust: RV64GC, Sv39 MMU, CLINT/PLIC/UART/virtio
//! block) is the "CPU"; it compiles to the same `wasm32-wasip2` target as the
//! rest of the fleet and steps under wasmtime in the TEE. The enclave pulls
//! the kernel + root filesystem from a configured S3 bucket over transparent
//! egress (SigV4 when credentials are set; unsigned for public buckets),
//! boots them, and streams the UART console to the browser over SSE; your
//! keystrokes POST back into the guest. Disk writes the guest makes can be
//! saved back to the bucket with a single PUT.
//!
//! This is a run-mode SERVICE app: `wasmtime run` + wasi:sockets, one live
//! process holding the machine in RAM, HTTP served on the loopback `http:`
//! port the enclave's TLS proxy forwards to (see network-test / the suite's
//! httpd.rs). The single thread interleaves CPU batches with HTTP polling.
//!
//! The guest also gets a virtio-net NIC terminated in user space by src/net.rs
//! (smoltcp): a DHCP server leases 10.0.2.15, and raw `tcp:` deployment ports
//! are spliced onto guest TCP connections (default tcp:2222 -> guest 22, so
//! `ssh -p 2222` reaches an sshd inside the machine). Outbound, the gateway
//! NATs guest flows onto real sockets slirp-style (TCP splices, per-flow UDP,
//! a DNS proxy at 10.0.2.2, gateway-answered ICMP echo), so `ping 8.8.8.8`
//! and `curl` work from the guest shell; `net.outbound: false` seals it.
//!
//! Routes:
//!   GET  /            console UI (self-contained HTML + embedded xterm)
//!   GET  /a/<asset>   embedded xterm.js / xterm.css
//!   GET  /status      JSON machine state (phase, image sizes, instret, MIPS)
//!   POST /start       {accessKeyId?,secretAccessKey?,sessionToken?,reset?}
//!                     fetch images from S3 (creds: body > config > unsigned)
//!                     and boot; reset:true re-fetches instead of using cache
//!   POST /input       raw bytes → the guest UART receive register
//!   POST /exec        {cmd,timeout_s?,max_bytes?} run a shell command on the
//!                     guest console and return its stdout + exit code (JSON)
//!   GET  /console     Server-Sent Events: base64 console output, scrollback first
//!   POST /save        dump the (guest-modified) disk and PUT it to saveKey
//!   POST /stop        halt the machine and drop it from RAM
//!   GET  /display     Server-Sent Events: the machine's screen as deflated
//!                     dirty bands (see display.rs) — the browser's monitor
//!   GET  /fb.png      the current frame as one PNG snapshot
//!   GET  /ping        liveness

mod display;
mod egress;
mod gz;
mod httpd;
mod net;
mod s3;
mod video;
mod worker;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use display::Display;
use httpd::{form_get, json, Request, Response, Server};
use net::{ForwardCfg, HostNet, NetStack};
use riscv_emu_rust::terminal::Terminal;
use riscv_emu_rust::Emulator;
use s3::{Creds, Endpoint};

static INDEX_HTML: &str = include_str!("index.html");
static XTERM_JS: &str = include_str!("vendor/xterm.js");
static XTERM_CSS: &str = include_str!("vendor/xterm.css");

const DEFAULT_PORT: u16 = 8000; // fleet policy: http:8000, never 8080
const MAX_BODY: usize = 256 * 1024;
const TICK_BATCH: u64 = 400_000; // CPU instructions per event-loop turn
const IDLE_BATCH: u64 = 4_000; // batch while the guest is parked in WFI: keeps
                               // timers/devices ticking at ~1% of the busy rate
                               // so an idle machine stops burning the host CPU
const SCROLLBACK: usize = 256 * 1024; // console bytes retained for late joiners
// Full-speed turns after network activity: ~100M instructions ≈ 1.25 guest
// seconds, enough to span a whole ping/keepalive cadence so an interactive
// network session never drops into the ~20x-slow idle clock mid-conversation.
const NET_BOOST_TURNS: u64 = 250;

// ---- config ---------------------------------------------------------------

struct Config {
    title: String,
    endpoint: String,
    region: String,
    bucket: String,
    kernel: String,
    fs: String,
    dtb: Option<String>,
    save_key: Option<String>,
    config_creds: Option<Creds>,
    autostart: bool,
    read_only: bool,
    net_enabled: bool,
    net_outbound: bool,
    forwards: Vec<ForwardCfg>,
    // Guest RAM in MiB (`ramMiB`). Default 512 keeps existing deployments'
    // footprint; the alpine/firefox image wants 1792. Clamped to [128, 1920]:
    // the emulator's RAM is one contiguous Vec and a wasm32 allocation caps at
    // 2 GiB, and a machine under 128 MiB can't even finish X startup.
    ram_mib: u64,
    // Display size (`display: {width, height}`), default 1024x768. Must fit the
    // DTB's 3 MiB framebuffer window; the emulator applies the same guard to
    // the device tree, so app and guest cannot disagree.
    fb_w: u64,
    fb_h: u64,
    // `realtime`: drive the guest's clock from the host's, instead of from
    // retired instructions. Anything that paces itself off the clock — a game,
    // a video player, a benchmark inside the guest — needs this to be true or
    // it runs at (emulated MIPS / 10) times speed.
    realtime: bool,
    api_key: Option<String>,
    // POST /exec: run a shell command on the guest's serial console and hand
    // back its stdout and exit code (see `exec`). It is the app-to-app control
    // channel — another enclave app can only reach this deployment over HTTP,
    // never ssh, so a command verb has to live here. Enabled by default; an
    // `exec` config object can turn it off, or set the serial login used when
    // the console sits at a getty rather than an already-open shell.
    exec_enabled: bool,
    exec_user: String,
    exec_password: Option<String>,
}

fn creds_from(v: &serde_json::Value) -> Option<Creds> {
    let ak = v.get("accessKeyId").and_then(|x| x.as_str())?;
    let sk = v.get("secretAccessKey").and_then(|x| x.as_str())?;
    if ak.is_empty() || sk.is_empty() {
        return None;
    }
    Some(Creds {
        access_key_id: ak.to_string(),
        secret_access_key: sk.to_string(),
        session_token: v
            .get("sessionToken")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Resolves config string values of the exact form `$NAME` / `${NAME}` from
/// the process environment. Deployment secrets arrive as env vars, so this is
/// what lets a config reference them instead of inlining values. Whole-value
/// references only, no interpolation inside larger strings. An unresolved
/// reference becomes "" (with a log line naming it), which downstream treats
/// as absent, so e.g. credentials fall back to the browser prompt.
fn expand_env_refs(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let Some(reference) = s.strip_prefix('$') else { return };
            let name = reference.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or(reference);
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.starts_with(|c: char| c.is_ascii_digit())
            {
                return; // not a $NAME reference; leave the literal alone
            }
            match std::env::var(name) {
                Ok(val) => {
                    eprintln!("[risc-box] config: resolved ${name} from the environment");
                    *s = val;
                }
                Err(_) => {
                    eprintln!("[risc-box] config: ${name} is not set in the environment; treating the value as absent");
                    s.clear();
                }
            }
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(expand_env_refs),
        serde_json::Value::Array(items) => items.iter_mut().for_each(expand_env_refs),
        _ => {}
    }
}

/// Reads the config, always returning one. Missing or unresolved fields are
/// left empty rather than fatal: a fresh deployment whose `$VAR` secrets are
/// not set yet must still START and serve the UI so they can be provided (and
/// the process restarted). Booting a machine checks `missing()` first.
fn load_config() -> Config {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("RISCBOX_CONFIG"))
        .unwrap_or_default();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        if !raw.is_empty() {
            eprintln!("[risc-box] config is not JSON ({e}); starting unconfigured");
        }
        serde_json::Value::Null
    });
    expand_env_refs(&mut v);
    let v = v;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    Config {
        title: s("title").unwrap_or_else(|| "RISC Box machine".to_string()),
        endpoint: s("endpoint").unwrap_or_default(),
        region: s("region").unwrap_or_else(|| "us-east-1".to_string()),
        bucket: s("bucket").unwrap_or_default(),
        kernel: s("kernel").unwrap_or_default(),
        fs: s("fs").unwrap_or_default(),
        dtb: s("dtb"),
        save_key: s("saveKey").or_else(|| s("fs")),
        config_creds: v.get("credentials").and_then(creds_from),
        autostart: v.get("autostart").and_then(|x| x.as_bool()).unwrap_or(false),
        read_only: v.get("readOnly").and_then(|x| x.as_bool()).unwrap_or(false),
        net_enabled: v.get("net").and_then(|x| x.as_bool()).unwrap_or(true),
        net_outbound: v
            .get("net")
            .and_then(|n| n.get("outbound"))
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
        forwards: forwards_from(v.get("net")),
        ram_mib: v
            .get("ramMiB")
            .and_then(|x| x.as_u64())
            .unwrap_or(512)
            .clamp(128, 1920),
        fb_w: v
            .get("display")
            .and_then(|d| d.get("width"))
            .and_then(|x| x.as_u64())
            .unwrap_or(1024),
        fb_h: v
            .get("display")
            .and_then(|d| d.get("height"))
            .and_then(|x| x.as_u64())
            .unwrap_or(768),
        realtime: v
            .get("realtime")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        // Optional shared secret. When set (directly or via a $VAR secret), the
        // control + observation endpoints require it; see `authorized`. Unset
        // means the deployment is open, which is only safe when it is private.
        api_key: s("api_key"),
        exec_enabled: v
            .get("exec")
            .and_then(|e| e.get("enabled"))
            .and_then(|x| x.as_bool())
            .unwrap_or(true),
        // The serial login /exec uses when the console is at a getty. Defaults
        // to a passwordless root, which is what the images in this repo present;
        // an image with a real password references a secret by name here.
        exec_user: v
            .get("exec")
            .and_then(|e| e.get("user"))
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .unwrap_or("root")
            .to_string(),
        exec_password: v
            .get("exec")
            .and_then(|e| e.get("password"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
    }
}

impl Config {
    /// The fields required to boot a machine. Any that are empty mean the
    /// deployment is not configured yet (typically an unresolved `$VAR`
    /// secret); the app still runs, it just can't fetch or boot until set.
    fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.endpoint.is_empty() {
            m.push("endpoint");
        }
        if self.bucket.is_empty() {
            m.push("bucket");
        }
        if self.kernel.is_empty() {
            m.push("kernel");
        }
        if self.fs.is_empty() {
            m.push("fs");
        }
        m
    }
}

/// The retained tail of the display's band stream, for GET /fb.bands. Bands
/// are deltas, so a puller must receive them contiguously — the ring hands
/// out events strictly from `since`+1, except that a retained FULL frame may
/// re-base a client that fell behind (a full band supersedes everything
/// before it, which is also why one clears the ring). A gap it cannot bridge
/// is answered with `resync`, and the caller schedules a full-frame scan.
struct BandRing {
    events: VecDeque<(u64, bool, String)>, // (gen, full-frame?, band JSON)
    gen: u64,
    bytes: usize,
}

impl BandRing {
    fn new() -> Self {
        BandRing { events: VecDeque::new(), gen: 0, bytes: 0 }
    }

    fn push(&mut self, ev: &str, full: bool) {
        self.gen += 1;
        if full {
            self.events.clear();
            self.bytes = 0;
        }
        self.bytes += ev.len();
        self.events.push_back((self.gen, full, ev.to_string()));
        // Bounded: a puller further behind than this is re-based or resynced.
        while self.bytes > (4 << 20) && self.events.len() > 1 {
            if let Some((_, _, e)) = self.events.pop_front() {
                self.bytes -= e.len();
            }
        }
    }

    /// Everything after `since`, contiguous, capped at `max_bytes` per reply
    /// (the caller polls again for the rest). Returns (last gen served,
    /// events, resync).
    fn since(&self, since: u64, max_bytes: usize) -> (u64, Vec<String>, bool) {
        if since >= self.gen {
            return (self.gen, Vec::new(), false);
        }
        let Some(&(first, first_full, _)) = self.events.front() else {
            return (since, Vec::new(), true);
        };
        let start = if since + 1 >= first {
            since + 1
        } else if first_full {
            first // the retained full frame re-bases the client
        } else {
            return (since, Vec::new(), true);
        };
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut last = since;
        for (g, _full, e) in &self.events {
            if *g < start {
                continue;
            }
            if bytes + e.len() > max_bytes && !out.is_empty() {
                break;
            }
            bytes += e.len();
            out.push(e.clone());
            last = *g;
        }
        (last, out, false)
    }
}

/// How long a /fb.bands?wait=1 request may stay parked before it is answered
/// empty. Short enough that the connection always ticks well inside every
/// proxy timeout, long enough that a 60 Hz consumer parks instead of polling.
const PULL_HOLD_MS: u64 = 150;

fn fb_bands_body(app: &mut App, since: u64) -> String {
    let (gen, events, resync) = app.pull.since(since, 240 * 1024);
    if resync {
        app.display.want_full();
        worker::want_full();
    }
    format!(
        "{{\"gen\":{},\"resync\":{},\"w\":{},\"h\":{},\"events\":[{}]}}",
        gen, resync, display::fb_w(), display::fb_h(), events.join(",")
    )
}

/// Whether a request may touch the machine. With no `api_key` configured the
/// app is open (fine for a private deployment). With one set, every control
/// and observation endpoint requires it — presented as `Authorization: Bearer
/// <key>`, `X-Api-Key: <key>`, or `?key=<key>` (the last for EventSource,
/// which cannot set headers). Without this a public deployment would hand any
/// passer-by a root console via /input and start/stop/save over the machine.
fn authorized(req: &Request, cfg: &Config) -> bool {
    let Some(want) = cfg.api_key.as_deref() else {
        return true;
    };
    if let Some(h) = req.header("authorization") {
        let tok = h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer "));
        if tok == Some(want) {
            return true;
        }
    }
    if req.header("x-api-key") == Some(want) {
        return true;
    }
    form_get(&req.query, "key").as_deref() == Some(want)
}

/// `net` config: absent or `true` → networking with the default ssh forward
/// (deployment tcp:2222 → guest 22) and outbound NAT; `false` → no NIC
/// backend; an object `{"forwards": [{"listen": 2222, "to": 22}, …],
/// "outbound": false}` → custom forwards and/or a sealed (inbound-only) net.
fn forwards_from(net: Option<&serde_json::Value>) -> Vec<ForwardCfg> {
    let default = vec![ForwardCfg { listen: 2222, to: 22 }];
    let Some(list) = net.and_then(|n| n.get("forwards")).and_then(|f| f.as_array()) else {
        return default;
    };
    let parsed: Vec<ForwardCfg> = list
        .iter()
        .filter_map(|f| {
            Some(ForwardCfg {
                listen: f.get("listen")?.as_u64()?.try_into().ok()?,
                to: f.get("to")?.as_u64()?.try_into().ok()?,
            })
        })
        .collect();
    match parsed.is_empty() {
        true => default,
        false => parsed,
    }
}

// ---- terminal: O(1) queues between the guest UART and HTTP -----------------

struct RiscBoxTerminal {
    input: VecDeque<u8>,
    output: VecDeque<u8>,
}
impl Terminal for RiscBoxTerminal {
    fn put_byte(&mut self, v: u8) {
        self.output.push_back(v);
    }
    fn get_output(&mut self) -> u8 {
        self.output.pop_front().unwrap_or(0)
    }
    fn put_input(&mut self, v: u8) {
        self.input.push_back(v);
    }
    fn get_input(&mut self) -> u8 {
        self.input.pop_front().unwrap_or(0)
    }
}

// ---- app state -------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Halted,
    Error,
}

struct Images {
    kernel: Vec<u8>,
    /// The root filesystem exactly as the bucket serves it — still gzipped
    /// when the key says so. Keeping the fetched form rather than the expanded
    /// one is what makes caching it affordable: this is held for the lifetime
    /// of the app so a restart need not re-download, and beside it the running
    /// machine already holds a full expanded copy of the same disk plus its
    /// DRAM. Cached compressed, a 320 MiB image costs 53 MiB here instead.
    fs_stored: Vec<u8>,
    fs_gzipped: bool,
    dtb: Option<Vec<u8>>,
}

impl Images {
    /// The disk to hand a fresh machine. Expanded per boot rather than held
    /// expanded, so the cost is paid only while a machine is actually running.
    /// The raw ext2 bytes to hand the emulator. Takes the stored image by
    /// value when it is already raw: a clone of a half-gigabyte fs is real
    /// wasm32 linear memory, and the cache is refilled on the next start.
    fn take_disk(&mut self) -> Result<Vec<u8>, String> {
        match self.fs_gzipped {
            true => gz::gunzip(&self.fs_stored),
            false => Ok(std::mem::take(&mut self.fs_stored)),
        }
    }
}

struct Start {
    creds: Option<Creds>,
    reset: bool,
}

struct App {
    cfg: Config,
    emu: Option<Emulator>,
    phase: Phase,
    error: Option<String>,
    pending: Option<Start>,
    cache: Option<Images>,
    live_creds: Option<Creds>, // remembered from the last successful start, for /save
    instret: u64,
    boot_at: Option<Instant>,
    // Presented frames per real second, sampled from the framebuffer's byte
    // counter (see `sample_fps`).
    fps_now: f64,
    fps_bytes: u64,
    fps_at: Instant,
    // Frames actually put on the wire for watchers (a scan that found any
    // change), and the rate derived from it. The guest's rate and this one are
    // different questions: the machine can draw faster than the scan is paced
    // to ship, and a viewer only ever sees this one.
    sent_frames: u64,
    sent_fps: f64,
    sent_at: Instant,
    sent_mark: u64,
    // Encoded video frames actually broadcast (the Moonlight-facing rate) and
    // the smoothed per-frame encode cost: together they say whether a 30 fps
    // stream is limited by the encoder or by the guest's paint rate.
    video_frames: u64,
    video_fps: f64,
    video_mark: u64,
    input_boost: u64, // turns to force full tick batches after POST /input
    exec_seq: u64,    // per-command nonce for /exec console markers
    scrollback: VecDeque<u8>,
    console_total: u64,
    last_save: Option<String>,
    net: Option<NetStack>, // listeners live for the whole process
    display: Display,      // scanout state (see display.rs)
    // Pull-paced frame delivery (GET /fb.bands): the ring retains the band
    // events the scan already produced, so a puller consumes at ITS OWN pace
    // with never more than one response in flight. Push-SSE cannot bound
    // latency on a link slower than production — the kernel and the relay
    // buffer megabytes the app cannot see, so a driving viewer (Moonlight)
    // sits seconds behind a perfectly smooth picture. One response in flight
    // caps that: current-but-choppier beats smooth-but-late.
    pull: BandRing,
    pull_seen: Option<Instant>, // a puller counts as a display watcher this recently
    // Long-polled /fb.bands?wait=1 requests: parked until the ring moves past
    // their `since` or the deadline lapses. The band leaves the app the
    // instant it exists instead of on the client's next poll — over a real
    // link that is the whole poll round trip saved, every frame.
    pull_waiters: Vec<(u64, u64, Instant)>, // (hold ticket, since, deadline)
    fb_scanned: Option<Instant>, // last display scan (paced by its own cost)
    fb_cost: Duration,           // smoothed cost of one display scan
    fb_still: u32,               // consecutive scans that found nothing
    // The /video stream's encoder (stateful, inter-frame) plus the packed
    // params it was built with; a codec/bitrate switch rebuilds it.
    venc: Option<(u32, Box<dyn video::VideoEncoder + Send>)>,
    video_scanned: Option<Instant>, // last /video frame (paced)
    /// Smoothed wall time one AV1 frame costs. The stream is paced off this
    /// rather than a fixed rate, so encoding can never take most of the thread
    /// the guest is running on (see the event loop).
    video_cost: Duration,
    // Boot-fetch retry: an image fetch can fail transiently — the platform's
    // egress front may reject the app's very first connect while the
    // deployment record is still mid-provision, S3 can blip — so a failed
    // start re-queues itself a few times before staying in Error. A fresh
    // operator /start resets the budget.
    retry: usize,
    retry_at: Option<Instant>,
    retry_start: Option<Start>,
}

/// Backoff for the boot-fetch retries (seconds between attempts).
const BOOT_RETRY_DELAYS: [u64; 4] = [5, 15, 30, 60];

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

impl App {
    /// Frames the guest has presented per real second, over the last sampling
    /// window (see `sample_fps`).
    ///
    /// A frame is `width * height * 4` bytes painted into the framebuffer, so
    /// this counts what the machine actually put on screen, not what it claims:
    /// the guest's own timing is only as honest as the guest's clock, and
    /// unless `realtime` is set that clock runs at (MIPS / 10) times speed.
    fn fps(&self) -> f64 {
        self.fps_now
    }

    fn mips(&self) -> f64 {
        match self.boot_at {
            Some(t) if self.instret > 0 => {
                let s = t.elapsed().as_secs_f64();
                if s > 0.0 { self.instret as f64 / 1e6 / s } else { 0.0 }
            }
            _ => 0.0,
        }
    }

    /// Push bytes into the guest's serial input, and keep the CPU on full
    /// batches long enough for the UART to drain them (it polls one byte per
    /// ~230k instructions). The same boost /input applies to a keystroke.
    fn push_input(&mut self, bytes: &[u8]) {
        if let Some(emu) = self.emu.as_mut() {
            let t = emu.get_mut_terminal();
            for &b in bytes {
                t.put_input(b);
            }
        }
        self.input_boost = self.input_boost.max(bytes.len() as u64 + 2);
    }

    fn status_json(&self) -> String {
        let phase = match self.phase {
            Phase::Idle => "idle",
            Phase::Running => "running",
            Phase::Halted => "halted",
            Phase::Error => "error",
        };
        let img = self
            .cache
            .as_ref()
            .map(|i| {
                format!(
                    ",\"kernelBytes\":{},\"fsBytes\":{},\"fsGzipped\":{}",
                    i.kernel.len(),
                    i.fs_stored.len(),
                    i.fs_gzipped
                )
            })
            .unwrap_or_default();
        format!(
            "{{\"phase\":\"{phase}\",\"title\":\"{}\",\"endpoint\":\"{}\",\"bucket\":\"{}\",\
             \"kernel\":\"{}\",\"fs\":\"{}\",\"saveKey\":{},\"readOnly\":{},\
             \"instret\":{},\"mips\":{:.1},\"fps\":{:.1},\"sentFps\":{:.1},\"videoFps\":{:.1},\"videoMs\":{:.1},\"capMs\":{:.2},\"display\":{{\"width\":{},\"height\":{},\"realtime\":{}}},\
             \"consoleBytes\":{},\"lastSave\":{},\"error\":{},\"net\":{},\"ramMiB\":{}{img}}}",
            httpd::json_escape(&self.cfg.title),
            httpd::json_escape(&self.cfg.endpoint),
            httpd::json_escape(&self.cfg.bucket),
            httpd::json_escape(&self.cfg.kernel),
            httpd::json_escape(&self.cfg.fs),
            self.cfg
                .save_key
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
            self.cfg.read_only,
            self.instret,
            self.mips(),
            self.fps(),
            self.sent_fps,
            self.video_fps,
            self.video_cost.as_secs_f64() * 1000.0,
            self.fb_cost.as_secs_f64() * 1000.0,
            display::fb_w(),
            display::fb_h(),
            self.cfg.realtime,
            self.console_total,
            self.last_save
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
            self.error
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
            self.net
                .as_ref()
                .map(|n| {
                    let fw: Vec<String> = n
                        .forwards()
                        .iter()
                        .map(|f| format!("{{\"listen\":{},\"to\":{}}}", f.listen, f.to))
                        .collect();
                    format!(
                        "{{\"guestIp\":\"{}.{}.{}.{}\",\"forwards\":[{}],\
                         \"rxFrames\":{},\"txFrames\":{},\"activeConns\":{},\
                         \"outbound\":{},\"natTcp\":{},\"natUdp\":{}}}",
                        net::GUEST_IP[0], net::GUEST_IP[1], net::GUEST_IP[2], net::GUEST_IP[3],
                        fw.join(","),
                        n.rx_frames, n.tx_frames, n.active_splices(),
                        n.outbound_enabled(), n.nat_tcp_flows(), n.nat_udp_flows()
                    )
                })
                .unwrap_or_else(|| "null".into()),
            self.cfg.ram_mib,
        )
    }
}

// ---- image fetch + boot ----------------------------------------------------

/// A fetch progress reporter that logs at every 10% of the object.
///
/// Image fetches are tens to hundreds of megabytes and they block the event
/// loop, so from outside the app a slow one is indistinguishable from a hang —
/// and on a private deployment, where the HTTP surface is not reachable, the
/// log is the only thing anyone can see. Ten lines per object is enough to
/// tell "downloading" from "stuck", and few enough to stay out of the way.
fn progress_logger(what: &str) -> impl FnMut(usize, usize) + '_ {
    let mut last_decile = 0usize;
    move |got, total| {
        if total == 0 {
            return;
        }
        let decile = got * 10 / total;
        if decile > last_decile {
            last_decile = decile;
            eprintln!("[risc-box]   {what}: {}% ({got}/{total} bytes)", decile * 10);
        }
    }
}

fn fetch_images(cfg: &Config, creds: Option<&Creds>) -> Result<Images, String> {
    let ep = Endpoint::parse(&cfg.endpoint, &cfg.region)?;
    let mut noop = |_: usize, _: usize| {};
    // Make the credential state explicit in the logs: a private bucket needs
    // signed requests, so "UNSIGNED" here next to an S3 4xx means the creds
    // never resolved (unset/misnamed secret), while a 401 on a SIGNED request
    // means the resolved key/secret is wrong (e.g. a rotated token).
    match creds.is_some() {
        true => eprintln!("[risc-box] S3 requests will be SIGNED (credentials resolved)"),
        false => eprintln!("[risc-box] S3 requests will be UNSIGNED (no credentials resolved; set config credentials, or use a public bucket)"),
    }
    eprintln!("[risc-box] fetching s3://{}/{}", cfg.bucket, cfg.kernel);
    let kernel = s3::get_object(&ep, &cfg.bucket, &cfg.kernel, creds, &mut noop)
        .map_err(|e| format!("fetch kernel {}: {e}", cfg.kernel))?;
    eprintln!("[risc-box]   kernel {} bytes; fetching {}", kernel.len(), cfg.fs);
    let mut fs_stored = s3::get_object(&ep, &cfg.bucket, &cfg.fs, creds, &mut progress_logger("fs"))
        .map_err(|e| format!("fetch fs {}: {e}", cfg.fs))?;
    // The download Vec doubles as it grows, so an image just past a power of
    // two carries up to 2x its size in dead capacity — real linear memory on
    // wasm32, where fs + guest RAM already crowd the budget. Return it.
    fs_stored.shrink_to_fit();
    // A `.gz` key is fetched and cached compressed and expanded per boot; see
    // Images::disk. Verify it inflates now rather than at boot, so a bad object
    // fails the fetch (which retries) instead of the machine start.
    let fs_gzipped = gz::is_gzip_key(&cfg.fs);
    match fs_gzipped {
        true => {
            let n = gz::gunzip(&fs_stored)?.len();
            eprintln!(
                "[risc-box]   fs {} bytes gzipped -> {} bytes ({:.1}x)",
                fs_stored.len(),
                n,
                n as f64 / fs_stored.len().max(1) as f64
            );
        }
        false => eprintln!("[risc-box]   fs {} bytes", fs_stored.len()),
    }
    let dtb = match &cfg.dtb {
        Some(k) => Some(
            s3::get_object(&ep, &cfg.bucket, k, creds, &mut noop)
                .map_err(|e| format!("fetch dtb {k}: {e}"))?,
        ),
        None => None,
    };
    Ok(Images { kernel, fs_stored, fs_gzipped, dtb })
}

fn boot(images: &mut Images, cfg: &Config) -> Result<Emulator, String> {
    let mut emu = Emulator::new(Box::new(RiscBoxTerminal {
        input: VecDeque::new(),
        output: VecDeque::new(),
    }));
    // before setup_program: that's where the RAM Vec is allocated and the
    // DTB memory node gets synced to it
    emu.setup_ram_bytes(cfg.ram_mib * 1024 * 1024);
    // Display size and clock source, both of which the guest reads exactly once
    // at boot: the DTB node for the framebuffer, the timebase for the clock.
    if cfg.fb_w != 1024 || cfg.fb_h != 768 {
        match emu.set_framebuffer_size(cfg.fb_w as u32, cfg.fb_h as u32)
            && display::set_size(cfg.fb_w as usize, cfg.fb_h as usize)
        {
            true => eprintln!("[risc-box] display {}x{}", cfg.fb_w, cfg.fb_h),
            false => eprintln!(
                "[risc-box] display {}x{} rejected (must be even and fit 3 MiB); staying at {}x{}",
                cfg.fb_w,
                cfg.fb_h,
                display::fb_w(),
                display::fb_h()
            ),
        }
    }
    if cfg.realtime {
        emu.set_wall_clock(true);
        eprintln!("[risc-box] guest clock: host monotonic (realtime)");
    }
    emu.setup_program(images.kernel.clone());
    emu.setup_filesystem(images.take_disk()?);
    if let Some(dtb) = &images.dtb {
        emu.setup_dtb(dtb.clone());
    }
    if cfg.net_enabled {
        emu.setup_network(Box::new(HostNet::new()));
    }
    Ok(emu)
}

// ---- request routing -------------------------------------------------------

fn route(app: &mut App, server: &mut Server, key: usize, req: Request) {
    // The static shell, its assets, and liveness stay open so the page can
    // load and prompt for a key; everything that reveals or drives the machine
    // is gated when api_key is set.
    let open = matches!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/") | ("GET", "/ping") | ("GET", "/a/xterm.js") | ("GET", "/a/xterm.css")
    );
    if !open && !authorized(&req, &app.cfg) {
        return server.respond(key, json(401, "Unauthorized", err("api key required")));
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "no-store")
                .body("text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()),
        ),
        ("GET", "/ping") => server.respond(key, json(200, "OK", "{\"ok\":true}".into())),
        ("GET", "/a/xterm.js") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "public, max-age=31536000, immutable")
                .body("text/javascript; charset=utf-8", XTERM_JS.as_bytes().to_vec()),
        ),
        ("GET", "/a/xterm.css") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "public, max-age=31536000, immutable")
                .body("text/css; charset=utf-8", XTERM_CSS.as_bytes().to_vec()),
        ),
        ("GET", "/status") => server.respond(key, json(200, "OK", app.status_json())),
        ("GET", "/console") => {
            // hand the late joiner the retained scrollback as the first frame
            let sb: Vec<u8> = app.scrollback.iter().copied().collect();
            let initial = if sb.is_empty() {
                String::new()
            } else {
                format!("data: {}\n\n", b64(&sb))
            };
            server.upgrade_sse(key, "console", &initial);
        }
        ("POST", "/start") => {
            if app.phase == Phase::Running {
                return server.respond(key, json(409, "Conflict", err("already running")));
            }
            let missing = app.cfg.missing();
            if !missing.is_empty() {
                return server.respond(key, json(400, "Bad Request", err(&format!(
                    "configuration incomplete: {} not set — set the deployment's config/secrets and restart",
                    missing.join(", ")
                ))));
            }
            let v: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
            let creds = creds_from(&v);
            let reset = v.get("reset").and_then(|x| x.as_bool()).unwrap_or(false);
            app.pending = Some(Start { creds, reset });
            app.retry = 0; // an operator start gets a fresh retry budget
            app.retry_at = None;
            app.retry_start = None;
            server.respond(key, json(202, "Accepted", "{\"ok\":true,\"phase\":\"loading\"}".into()));
        }
        ("POST", "/input") => {
            if let (Phase::Running, Some(emu)) = (app.phase, app.emu.as_mut()) {
                let t = emu.get_mut_terminal();
                for &b in &req.body {
                    t.put_input(b);
                }
                // run full batches until the UART has had time to drain this
                // input (it polls its terminal every ~230k ticks, one byte per
                // poll), else the idle throttle would add ~100ms per keystroke
                app.input_boost = app.input_boost.max(req.body.len() as u64 + 2);
                server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
            } else {
                server.respond(key, json(409, "Conflict", err("machine is not running")));
            }
        }
        ("POST", "/hid") => hid(app, server, key, &req.body),
        // The streamed variant: this request never ends and is never answered;
        // each newline-delimited body line arrives back through poll() as a
        // synthesized /hid-stream-event and is injected with zero per-batch
        // framing or response work (see httpd::upgrade_instream).
        ("POST", "/hid-stream") => server.upgrade_instream(key, "/hid-stream-event"),
        ("POST", "/hid-stream-event") => hid_inner(app, server, key, &req.body, false),
        ("POST", "/exec") => exec(app, server, key, &req.body),
        ("POST", "/save") => save(app, server, key),
        ("POST", "/stop") => {
            app.emu = None;
            app.phase = Phase::Halted;
            app.boot_at = None;
            app.display.reset();
            worker::reset();
            app.venc = None;
            server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
        }
        ("GET", "/display") => {
            // the machine's screen: metadata first, then bands (display.rs).
            // The joiner needs the WHOLE frame once — force it on the next
            // scan (a broadcast reaches existing watchers too; a duplicate
            // full band is idempotent on a canvas).
            app.display.want_full();
            worker::want_full();
            let initial = format!(
                "event: mode\ndata: {{\"w\":{},\"h\":{}}}\n\n",
                display::fb_w(), display::fb_h()
            );
            server.upgrade_sse(key, "display", &initial);
        }
        // The efficient video stream: the guest desktop encoded as AV1 in the
        // app (rav1e, inter-frame), each coded frame shipped base64 over SSE.
        // The browser decodes it with WebCodecs (see index.html). A fresh
        // watcher forces a new encoder (below), so the first frame it sees is a
        // keyframe. `event: codec` carries the WebCodecs codec string + size.
        ("GET", "/video") => {
            // `?codec=h264` selects the Moonlight-native stream (Annex-B over
            // the same base64 SSE), `av1` (the default) stays the browser's
            // WebCodecs codec. One encoder exists at a time: the codec of the
            // most recent joiner wins and the switch rebuilds it, so the new
            // stream leads with a keyframe either way. `?kbps=` sets the VBV
            // target (defaults: 3000 h264, 4000 av1).
            let codec = match form_get(&req.query, "codec").as_deref() {
                Some("h264") => worker::CODEC_H264,
                _ => worker::CODEC_AV1,
            };
            let kbps = form_get(&req.query, "kbps")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            worker::set_video_params(codec, kbps);
            app.venc = None; // rebuild inline too, so the joiner gets an IDR
            worker::reset();
            let codec_str = match codec {
                worker::CODEC_H264 => "avc1.42E020",
                _ => "av01.0.08M.08",
            };
            let initial = format!(
                "event: codec\ndata: {{\"codec\":\"{}\",\"w\":{},\"h\":{}}}\n\n",
                codec_str, display::fb_w(), display::fb_h()
            );
            server.upgrade_sse(key, "video", &initial);
        }
        // A stream consumer lost packets and needs a random-access point (the
        // Moonlight bridge forwards the client's IDR requests here).
        ("POST", "/video-key") => {
            worker::force_key();
            if let Some((_, enc)) = app.venc.as_mut() {
                enc.force_keyframe();
            }
            server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
        }
        ("GET", "/fb.png") => match (app.phase, app.emu.as_ref()) {
            (Phase::Running, Some(emu)) | (Phase::Halted, Some(emu)) => {
                let png = app.display.png(emu);
                server.respond(
                    key,
                    Response::new(200, "OK")
                        .with("cache-control", "no-store")
                        .body("image/png", png),
                );
            }
            _ => server.respond(key, json(409, "Conflict", err("machine is not running"))),
        },
        // The video encode path (src/video.rs): one encoded frame of the guest
        // desktop, encoded IN THIS APP (wasm-JIT speed), not the emulated
        // guest. Motion JPEG today; the VideoEncoder seam is where an
        // H.264/NVENC-on-H200 backend drops in. `?q=` sets JPEG quality.
        ("GET", "/frame.jpg") => match (app.phase, app.emu.as_ref()) {
            (Phase::Running, Some(emu)) | (Phase::Halted, Some(emu)) => {
                use video::VideoEncoder;
                let q = form_get(&req.query, "q").and_then(|v| v.parse::<u8>().ok()).unwrap_or(75);
                let (rgb, w, h) = video::capture_rgb(emu);
                let mut enc = video::MjpegEncoder::new(q);
                let mime = enc.mime();
                let data = enc.encode(&rgb, w, h).pop().map(|f| f.data).unwrap_or_default();
                server.respond(
                    key,
                    Response::new(200, "OK")
                        .with("cache-control", "no-store")
                        .body(mime, data),
                );
            }
            _ => server.respond(key, json(409, "Conflict", err("machine is not running"))),
        },
        // Raw framebuffer (packed RGB, FB_W x FB_H, no header) — the frame source
        // for a HARDWARE encoder. The wasm app can't call NVENC, so the native
        // GPU bridge (gs-bridge) pulls raw frames here and NVENC-encodes them
        // on the GPU (the H200 on the fleet; a dev GPU locally). This is the
        // "GPU compute for the RISC Box app runs on the H200" path — the encode
        // is offloaded off the emulated CPU AND off this wasm app to the GPU.
        // Pull-paced frame delivery: the band events the scan already
        // produced, from `since` on, in one bounded response. The client's
        // in-flight window is one reply deep, so its latency is its own
        // link's, not the megabytes of relay buffering a pushed stream
        // accumulates when production outruns the link (which is what made
        // a driven cursor sit seconds behind a smooth picture). `resync`
        // tells the client its `since` fell out of the ring; a full-frame
        // scan is already scheduled and a later poll re-bases it.
        ("GET", "/fb.bands") => {
            app.pull_seen = Some(Instant::now());
            let since = form_get(&req.query, "since")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let wait = form_get(&req.query, "wait").as_deref() == Some("1");
            // Nothing to say yet and the client offered to wait: park it.
            // The release pass in the main loop answers the moment the ring
            // moves (or after PULL_HOLD_MS, so the connection always ticks).
            if wait && since >= app.pull.gen {
                if let Some(ticket) = server.hold(key) {
                    app.pull_waiters.push((
                        ticket,
                        since,
                        Instant::now() + Duration::from_millis(PULL_HOLD_MS),
                    ));
                    return;
                }
            }
            let body = fb_bands_body(app, since);
            server.respond(
                key,
                Response::new(200, "OK")
                    .with("cache-control", "no-store")
                    .body("application/json", body.into_bytes()),
            );
        }
        ("GET", "/fb.rgb") => match (app.phase, app.emu.as_ref()) {
            (Phase::Running, Some(emu)) | (Phase::Halted, Some(emu)) => {
                let (rgb, _w, _h) = video::capture_rgb(emu);
                server.respond(
                    key,
                    Response::new(200, "OK")
                        .with("cache-control", "no-store")
                        .body("application/octet-stream", rgb),
                );
            }
            _ => server.respond(key, json(409, "Conflict", err("machine is not running"))),
        },
        _ => server.respond(key, json(404, "Not Found", err("no such route"))),
    }
}

fn err(msg: &str) -> String {
    format!("{{\"error\":{{\"message\":\"{}\"}}}}", httpd::json_escape(msg))
}

// Linux input-event-codes the /hid endpoint speaks (mirror of the set the
// emulator's virtio-input device advertises).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

/// POST /hid — inject pointer/keyboard input into the machine's virtio-input
/// device. Body: {"events":[ … ]} where each event is one of
///   {"t":"move","x":0.0..1.0,"y":0.0..1.0}   absolute pointer (normalized)
///   {"t":"moveabs","ax":0..32767,"ay":…}     absolute pointer (raw axis units)
///   {"t":"button","b":"left|right|middle","down":true|false}
///   {"t":"key","code":<linux keycode>,"down":true|false}
///   {"t":"scroll","dy":<notches>,"dx":<notches>}
/// Each event is committed to the guest with an EV_SYN report. This is the
/// interface a remote/streaming client drives the desktop through; it is also
/// the hook a GameStream host's input backend targets.
fn hid(app: &mut App, server: &mut Server, key: usize, body: &[u8]) {
    hid_inner(app, server, key, body, true)
}

fn hid_inner(app: &mut App, server: &mut Server, key: usize, body: &[u8], respond: bool) {
    if app.phase != Phase::Running || app.emu.is_none() {
        if respond {
            server.respond(key, json(409, "Conflict", err("machine is not running")));
        }
        return;
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            if respond {
                server.respond(key, json(400, "Bad Request", err(&format!("bad JSON: {e}"))));
            }
            return;
        }
    };
    let Some(events) = v.get("events").and_then(|e| e.as_array()) else {
        if respond {
            server.respond(key, json(400, "Bad Request", err("expected {\"events\":[…]}")));
        }
        return;
    };
    let emu = app.emu.as_mut().expect("emu present (checked above)");
    let abs_max = Emulator::input_abs_max() as f64;
    let mut n = 0u32;
    let syn = |emu: &mut Emulator| emu.push_input_event(EV_SYN, 0, 0);
    for ev in events {
        let kind = ev.get("t").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "move" => {
                let x = ev.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
                let y = ev.get("y").and_then(|y| y.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
                emu.push_input_event(EV_ABS, ABS_X, (x * abs_max).round() as u32);
                emu.push_input_event(EV_ABS, ABS_Y, (y * abs_max).round() as u32);
                syn(emu);
                n += 1;
            }
            "moveabs" => {
                let ax = ev.get("ax").and_then(|x| x.as_i64()).unwrap_or(0).clamp(0, abs_max as i64);
                let ay = ev.get("ay").and_then(|y| y.as_i64()).unwrap_or(0).clamp(0, abs_max as i64);
                emu.push_input_event(EV_ABS, ABS_X, ax as u32);
                emu.push_input_event(EV_ABS, ABS_Y, ay as u32);
                syn(emu);
                n += 1;
            }
            "button" => {
                let code = match ev.get("b").and_then(|b| b.as_str()).unwrap_or("left") {
                    "right" => BTN_RIGHT,
                    "middle" => BTN_MIDDLE,
                    _ => BTN_LEFT,
                };
                let down = ev.get("down").and_then(|d| d.as_bool()).unwrap_or(true);
                emu.push_input_event(EV_KEY, code, down as u32);
                syn(emu);
                n += 1;
            }
            "key" => {
                let code = ev.get("code").and_then(|c| c.as_u64()).unwrap_or(0) as u16;
                let down = ev.get("down").and_then(|d| d.as_bool()).unwrap_or(true);
                if code != 0 {
                    emu.push_input_event(EV_KEY, code, down as u32);
                    syn(emu);
                    n += 1;
                }
            }
            "scroll" => {
                let dy = ev.get("dy").and_then(|d| d.as_i64()).unwrap_or(0);
                let dx = ev.get("dx").and_then(|d| d.as_i64()).unwrap_or(0);
                if dy != 0 {
                    emu.push_input_event(EV_REL, REL_WHEEL, dy as i32 as u32);
                }
                if dx != 0 {
                    emu.push_input_event(EV_REL, REL_HWHEEL, dx as i32 as u32);
                }
                if dy != 0 || dx != 0 {
                    syn(emu);
                    n += 1;
                }
            }
            _ => {}
        }
    }
    // Run full CPU batches for a bit so the guest services the input IRQ and
    // X repaints promptly instead of at the idle-throttle rate.
    app.input_boost = app.input_boost.max(NET_BOOST_TURNS);
    if respond {
        server.respond(key, json(200, "OK", format!("{{\"ok\":true,\"events\":{n}}}")));
    }
}

// ---- POST /exec: run a shell command on the guest serial console -----------

/// Ceiling on how long one /exec may hold the event loop. A command runs the
/// CPU inline (the way /start's image fetch does), so nothing else is serviced
/// while it runs; keep it well under the platform gateway's ~180s idle cut. The
/// request's own timeout is clamped to this, and covers login + the command.
const EXEC_MAX_TIMEOUT_S: u64 = 120;
const EXEC_DEFAULT_TIMEOUT_S: u64 = 30;
const EXEC_DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// POST /exec — run a shell command on the guest and return its stdout and exit
/// code. Body: {"cmd":"<shell>", "timeout_s"?:1..120, "max_bytes"?:<output cap>}.
/// Answers {"ok":true,"exitCode":N,"output":"…","truncated":bool,"ms":N}, or
/// {"ok":false,"error":"…","output":"<what came back>"} on a timeout or a
/// console that never reached a prompt. 409 when the machine is not running,
/// 403 when exec is disabled.
///
/// There is no exec channel inside the guest for a caller to reach — an enclave
/// app can only speak HTTP to this deployment, never ssh — so this is built from
/// the serial console the way scripts/bench.py drives it, moved server-side
/// where the fiddly parts belong. The command is base64-wrapped so the single
/// line written to the UART carries arbitrary bytes, quotes and newlines intact,
/// and it is bracketed by two `printf` markers whose unique tag is passed as a
/// printf ARGUMENT: the command line the tty echoes back contains the format
/// string, never the expanded marker, so scanning the console for the marker
/// can never match our own echo. stdout is everything printed between the two
/// markers; the exit code rides the closing one.
///
/// It blocks the event loop until the command finishes or the timeout fires,
/// stepping the CPU inline and pumping the guest NIC so a networked command
/// still works, broadcasting the same bytes to console watchers, and flushing
/// periodically so SSE heartbeats keep going out.
fn exec(app: &mut App, server: &mut Server, key: usize, body: &[u8]) {
    if !app.cfg.exec_enabled {
        return server.respond(key, json(403, "Forbidden", err("exec is disabled on this deployment")));
    }
    if app.phase != Phase::Running || app.emu.is_none() {
        return server.respond(key, json(409, "Conflict", err("machine is not running")));
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return server.respond(key, json(400, "Bad Request", err(&format!("bad JSON: {e}")))),
    };
    let Some(cmd) = v.get("cmd").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) else {
        return server.respond(key, json(400, "Bad Request", err("expected {\"cmd\": \"<shell command>\"}")));
    };
    let timeout = Duration::from_secs(
        v.get("timeout_s")
            .and_then(|x| x.as_u64())
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_S)
            .clamp(1, EXEC_MAX_TIMEOUT_S),
    );
    let max_out = v
        .get("max_bytes")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize)
        .unwrap_or(EXEC_DEFAULT_MAX_BYTES)
        .clamp(1024, MAX_BODY);

    let seq = app.exec_seq;
    app.exec_seq = app.exec_seq.wrapping_add(1);
    let tag = format!("RBX{seq}Z");
    let begin = format!("{tag}B");
    let end = format!("{tag}E:");
    // One line, the command base64'd inside it so its own bytes never need
    // escaping and can hold newlines. The tag is a printf argument, so the
    // echoed line shows `'RBX0Z'` and `%sB`/`%sE:%s`, never `RBX0ZB`/`RBX0ZE:`.
    let line = format!(
        "printf '%sB\\n' '{tag}'; printf %s '{}' | base64 -d | sh; __rc=$?; printf '%sE:%s\\n' '{tag}' \"$__rc\"\n",
        b64(cmd.as_bytes())
    );

    let user = app.cfg.exec_user.clone();
    let pass = app.cfg.exec_password.clone().unwrap_or_default();
    let began = Instant::now();
    let mut cap: Vec<u8> = Vec::new();
    let mut last_flush = began;

    // Phase 1 — reach a shell prompt. A bare newline draws a prompt from an
    // open shell, or `login:` from a getty, which we answer from the configured
    // (passwordless-root by default) credentials. The command is only sent once
    // a prompt is in hand, so a login prompt can never consume it as a username.
    app.push_input(b"\n");
    let ready_budget = (timeout / 2).min(Duration::from_secs(10));
    let (mut sent_user, mut sent_pass, mut ready) = (false, false, false);
    while began.elapsed() < ready_budget {
        exec_pump(app, server, &mut cap, &mut last_flush);
        if tail_is_prompt(&cap) {
            ready = true;
            break;
        }
        if !sent_user && contains(&cap, b"ogin:") {
            app.push_input(format!("{user}\n").as_bytes());
            sent_user = true;
        } else if sent_user && !sent_pass && contains(&cap, b"assword:") {
            app.push_input(format!("{pass}\n").as_bytes());
            sent_pass = true;
        }
    }
    if !ready {
        let tail = &cap[cap.len().saturating_sub(800)..];
        return server.respond(
            key,
            json(200, "OK", format!(
                "{{\"ok\":false,\"error\":\"{}\",\"output\":\"{}\",\"ms\":{}}}",
                httpd::json_escape(&format!(
                    "guest shell not ready: no prompt appeared on the serial console within {}s (is a getty running on ttyS0, or is the guest still booting?)",
                    ready_budget.as_secs()
                )),
                httpd::json_escape(&String::from_utf8_lossy(tail)),
                began.elapsed().as_millis()
            )),
        );
    }

    // Phase 2 — send the command and wait for the closing marker (with its
    // whole exit-code line, i.e. a newline after it).
    let cmd_off = cap.len();
    app.push_input(line.as_bytes());
    let mut end_at: Option<usize> = None;
    while began.elapsed() < timeout {
        exec_pump(app, server, &mut cap, &mut last_flush);
        if let Some(ei) = find_from(&cap, end.as_bytes(), cmd_off) {
            if find_from(&cap, b"\n", ei + end.len()).is_some() {
                end_at = Some(ei);
                break;
            }
        }
    }

    // Everything the command printed starts after the begin marker's own line.
    let out_start = find_from(&cap, begin.as_bytes(), cmd_off)
        .and_then(|bi| find_from(&cap, b"\n", bi).map(|nl| nl + 1))
        .unwrap_or(cmd_off);

    let (ok, exit_code, out_end) = match end_at {
        Some(ei) => {
            let rc: i64 = cap[ei + end.len()..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(String::new(), |mut s, &b| {
                    s.push(b as char);
                    s
                })
                .parse()
                .unwrap_or(-1);
            (true, rc, ei)
        }
        None => (false, -1, cap.len()),
    };

    let mut text = String::from_utf8_lossy(&cap[out_start.min(out_end)..out_end])
        .replace("\r\n", "\n");
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    let truncated = text.len() > max_out;
    if truncated {
        let mut n = max_out;
        while n > 0 && !text.is_char_boundary(n) {
            n -= 1;
        }
        text.truncate(n);
    }

    let payload = if ok {
        format!(
            "{{\"ok\":true,\"exitCode\":{exit_code},\"output\":\"{}\",\"truncated\":{truncated},\"ms\":{}}}",
            httpd::json_escape(&text),
            began.elapsed().as_millis()
        )
    } else {
        format!(
            "{{\"ok\":false,\"error\":\"{}\",\"output\":\"{}\",\"truncated\":{truncated},\"ms\":{}}}",
            httpd::json_escape(&format!("exec timed out after {}s", timeout.as_secs())),
            httpd::json_escape(&text),
            began.elapsed().as_millis()
        )
    };
    server.respond(key, json(200, "OK", payload));
}

/// One event-loop turn's worth of guest work, for the inline /exec wait: step
/// the CPU, drain the UART into the console (scrollback + SSE + the capture
/// buffer), pump the NIC, and periodically flush so SSE heartbeats still fire.
fn exec_pump(app: &mut App, server: &mut Server, cap: &mut Vec<u8>, last_flush: &mut Instant) {
    let mut chunk: Vec<u8> = Vec::new();
    if let Some(emu) = app.emu.as_mut() {
        emu.run_n(TICK_BATCH);
        app.instret += TICK_BATCH;
        let t = emu.get_mut_terminal();
        loop {
            let b = t.get_output();
            if b == 0 {
                break;
            }
            chunk.push(b);
            if chunk.len() >= 64 * 1024 {
                break;
            }
        }
        // keep forwarded/outbound guest connections alive across a networked cmd
        if let Some(stack) = app.net.as_mut() {
            let backend = emu.get_mut_cpu().get_mut_mmu().get_mut_net().get_mut_backend();
            stack.pump(backend.as_mut());
        }
    }
    if !chunk.is_empty() {
        app.console_total += chunk.len() as u64;
        for &b in &chunk {
            if app.scrollback.len() >= SCROLLBACK {
                app.scrollback.pop_front();
            }
            app.scrollback.push_back(b);
        }
        server.broadcast("console", &format!("data: {}", b64(&chunk)));
        cap.extend_from_slice(&chunk);
    }
    if last_flush.elapsed() >= Duration::from_millis(50) {
        server.flush();
        *last_flush = Instant::now();
    }
}

/// The tail of the console looks like a shell prompt waiting for input: the
/// last non-blank line ends in one of the usual prompt characters. Enough to
/// tell "ready for a command" from "still booting"; a false positive only sends
/// the command a beat early, and the marker wait absorbs that.
fn tail_is_prompt(cap: &[u8]) -> bool {
    let start = cap.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    // Strip ANSI escape sequences before testing: busybox ash with terminal
    // line editing prints its prompt and then asks the terminal where the
    // cursor is (ESC[6n) — nobody here answers, but the query must not hide
    // the prompt character from this check ("~ # \x1b[6n" IS a prompt).
    let mut line = Vec::with_capacity(cap.len() - start);
    let mut i = start;
    while i < cap.len() {
        if cap[i] == 0x1b {
            i += 1;
            if i < cap.len() && cap[i] == b'[' {
                i += 1;
                while i < cap.len() && !cap[i].is_ascii_alphabetic() {
                    i += 1;
                }
                i += 1; // the final letter
            }
            continue;
        }
        line.push(cap[i]);
        i += 1;
    }
    match line.iter().rposition(|&b| b != b' ' && b != b'\r' && b != b'\t') {
        Some(i) => matches!(line[i], b'#' | b'$' | b'>'),
        None => false,
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find_from(hay, needle, 0).is_some()
}

/// First index of `needle` in `hay` at or after `from`.
fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let from = from.min(hay.len());
    if needle.is_empty() || needle.len() > hay.len() - from {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn save(app: &mut App, server: &mut Server, key: usize) {
    if app.cfg.read_only {
        return server.respond(key, json(403, "Forbidden", err("this machine is read-only")));
    }
    let Some(save_key) = app.cfg.save_key.clone() else {
        return server.respond(key, json(400, "Bad Request", err("no saveKey configured")));
    };
    let Some(emu) = app.emu.as_mut() else {
        return server.respond(key, json(409, "Conflict", err("machine is not running")));
    };
    let disk = emu.get_mut_cpu().get_mut_mmu().get_disk().dump_contents();
    let ep = match Endpoint::parse(&app.cfg.endpoint, &app.cfg.region) {
        Ok(e) => e,
        Err(e) => return server.respond(key, json(500, "Error", err(&e))),
    };
    // Match the object to the name it is being stored under. saveKey falls
    // back to the fs key, so a machine booted from a `.gz` image would
    // otherwise write its disk back raw under a `.gz` name — bootable exactly
    // once more, then permanently "bad magic".
    let raw_len = disk.len();
    let disk = match gz::is_gzip_key(&save_key) {
        true => gz::gzip(&disk),
        false => disk,
    };
    // flush the 202-less response path: PUT blocks the loop, like /start's fetch
    match raw_len == disk.len() {
        true => eprintln!("[risc-box] saving {raw_len} bytes to s3://{}/{save_key}", app.cfg.bucket),
        false => eprintln!(
            "[risc-box] saving {raw_len} bytes gzipped to {} to s3://{}/{save_key}",
            disk.len(),
            app.cfg.bucket
        ),
    }
    match s3::put_object(&ep, &app.cfg.bucket, &save_key, app.live_creds.as_ref(), &disk) {
        Ok(()) => {
            app.last_save = Some(save_key.clone());
            server.respond(
                key,
                json(200, "OK", format!("{{\"ok\":true,\"saved\":\"{}\",\"bytes\":{}}}",
                    httpd::json_escape(&save_key), disk.len())),
            )
        }
        Err(e) => server.respond(key, json(502, "Bad Gateway", err(&e))),
    }
}

/// Perform a queued /start: fetch (or reuse cached) images and boot.
fn do_start(app: &mut App, start: Start) {
    let need_fetch = start.reset || app.cache.is_none();
    if need_fetch {
        // creds precedence: request body > config; borrow-safe clone of config creds
        let body = start.creds;
        let chosen = body.as_ref().or(app.cfg.config_creds.as_ref());
        // stash which creds we used so /save can reuse them
        match fetch_images(&app.cfg, chosen) {
            Ok(imgs) => {
                app.live_creds = match &body {
                    Some(c) => Some(clone_creds(c)),
                    None => app.cfg.config_creds.as_ref().map(clone_creds),
                };
                app.cache = Some(imgs);
                app.retry = 0;
                app.retry_at = None;
                app.retry_start = None;
            }
            Err(e) => {
                eprintln!("[risc-box] start failed: {e}");
                if app.retry < BOOT_RETRY_DELAYS.len() {
                    let delay = BOOT_RETRY_DELAYS[app.retry];
                    eprintln!(
                        "[risc-box] retrying fetch in {delay}s (attempt {}/{})",
                        app.retry + 1,
                        BOOT_RETRY_DELAYS.len()
                    );
                    app.retry_at = Some(Instant::now() + Duration::from_secs(delay));
                    app.retry_start = Some(Start {
                        creds: body.as_ref().map(clone_creds),
                        reset: start.reset,
                    });
                }
                app.error = Some(e);
                app.phase = Phase::Error;
                return;
            }
        }
    }
    let imgs = app.cache.as_mut().expect("cache present after fetch");
    // Expanding a gzipped image can fail (corrupt object, or no room for the
    // expanded disk beside everything else). Treat it exactly like a failed
    // fetch: report it and leave the machine stopped, rather than unwrapping
    // and taking the whole app down with it.
    let emu = match boot(imgs, &app.cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[risc-box] start failed: {e}");
            app.error = Some(e);
            app.phase = Phase::Error;
            return;
        }
    };
    app.emu = Some(emu);
    app.instret = 0;
    app.boot_at = Some(Instant::now());
    app.scrollback.clear();
    app.console_total = 0;
    app.error = None;
    app.display.reset(); // fresh machine, fresh screen: next watched scan ships a full frame
    worker::reset();
    app.venc = None; // fresh machine, fresh encoder (next watcher gets a keyframe)
    app.phase = Phase::Running;
    eprintln!("[risc-box] machine running: {}", app.cfg.title);
}

fn clone_creds(c: &Creds) -> Creds {
    Creds {
        access_key_id: c.access_key_id.clone(),
        secret_access_key: c.secret_access_key.clone(),
        session_token: c.session_token.clone(),
    }
}

/// The C entry point for the shared-everything-threads build, where the final
/// link is done by the SET clang wrapper around a C `main` and Rust is a
/// staticlib. Same `run()` the ordinary bin calls.
#[no_mangle]
pub extern "C" fn risc_box_main() -> i32 {
    run();
    0
}

pub fn run() {
    // What the platform actually handed us, by NAME only — never a value.
    //
    // A deployment whose $NAME placeholders come out unresolved has two very
    // different causes that look identical from the config alone: the platform
    // substituted nothing because it had no secrets, or it had them and the
    // env never reached this process. One line here separates those, and
    // without it the answer costs a day of tracing across three machines.
    // Names are already public (they are in the app config); values are not,
    // and never appear here.
    // Say which build this is, first line, before anything can go wrong.
    //
    // Nothing in the logs or /status identified the running build, so "which
    // version is that deployment actually on" had to be answered by reading a
    // catalog index back through the ledger — and during a rollout, when two
    // versions differ by one changed default, that is exactly the question
    // being asked. The threading model is on the same line because it decides
    // whether watching the machine costs the machine, and it is a build-time
    // fact, not a config one.
    eprintln!(
        "[risc-box] risc-box {} ({} build)",
        env!("CARGO_PKG_VERSION"),
        match cfg!(feature = "set") {
            true => "shared-everything-threads",
            false => "single-threaded wasip2",
        }
    );

    let mut names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    names.sort();
    eprintln!("[risc-box] guest env ({}): {}", names.len(), names.join(" "));

    // Never exit on config problems: a fresh deployment whose $VAR secrets are
    // not set yet must still come up so the operator can set them (and restart)
    // rather than the whole deployment landing in "failed".
    let cfg = load_config();
    let missing = cfg.missing();
    let unconfigured = !missing.is_empty();
    if unconfigured {
        eprintln!(
            "[risc-box] starting UNCONFIGURED: {} not set — serving the UI; set the deployment's config/secrets and restart to boot",
            missing.join(", ")
        );
    }
    // Only autostart a fully-configured machine.
    let autostart = cfg.autostart && !unconfigured;
    // Bring a display worker up if the build and the engine can. When they
    // cannot this returns false and every watcher path stays inline, which is
    // exactly the app that shipped before.
    worker::start();
    let mut server = Server::bind("risc-box", DEFAULT_PORT);
    let mut app = App {
        cfg,
        emu: None,
        phase: Phase::Idle,
        error: if unconfigured {
            Some(format!(
                "configuration incomplete: {} not set — set the deployment's config/secrets and restart",
                missing.join(", ")
            ))
        } else {
            None
        },
        pending: if autostart { Some(Start { creds: None, reset: false }) } else { None },
        cache: None,
        live_creds: None,
        instret: 0,
        boot_at: None,
        fps_now: 0.0,
        fps_bytes: 0,
        fps_at: Instant::now(),
        sent_frames: 0,
        sent_fps: 0.0,
        video_frames: 0,
        video_fps: 0.0,
        video_mark: 0,
        sent_at: Instant::now(),
        sent_mark: 0,
        input_boost: 0,
        exec_seq: 0,
        scrollback: VecDeque::new(),
        console_total: 0,
        last_save: None,
        net: None,
        display: Display::new(),
        pull: BandRing::new(),
        pull_seen: None,
        pull_waiters: Vec::new(),
        fb_scanned: None,
        fb_cost: Duration::from_millis(0),
        fb_still: 0,
        venc: None,
        video_scanned: None,
        video_cost: Duration::from_millis(0),
        retry: 0,
        retry_at: None,
        retry_start: None,
    };
    if app.cfg.net_enabled {
        app.net = Some(NetStack::new(&app.cfg.forwards, app.cfg.net_outbound));
        match app.cfg.net_outbound {
            true => eprintln!("[risc-box] net: outbound NAT enabled (tcp/udp/dns/icmp-echo); disable with net.outbound=false"),
            false => eprintln!("[risc-box] net: outbound disabled — inbound forwards only"),
        }
    }

    // Periodic health line. The HTTP surface already reports all of this, but
    // it is not always reachable: a PRIVATE deployment has no public data path,
    // so its log is the only window into it, and that is exactly when you most
    // want to know whether the guest is running, wedged, or quietly stopped.
    // One line a minute is cheap enough to leave on always.
    const HEARTBEAT: Duration = Duration::from_secs(60);
    let mut last_heartbeat = Instant::now();

    loop {
        for (key, req) in server.poll(MAX_BODY) {
            route(&mut app, &mut server, key, req);
        }

        // A subscriber that fell behind was starved rather than closed
        // (httpd.rs, SSE_SKIP_WBUF); the events it missed were dropped, not
        // queued. Now that it has drained it owes nothing — WE owe it a
        // complete picture: a whole-frame scan for the display, a fresh
        // encoder (hence keyframe) for the video stream. Console watchers
        // just lose the gap; scrollback only ever replays on join.
        if server.sse_take_recovered("display") {
            app.display.want_full();
            worker::want_full();
        }
        if server.sse_take_recovered("video") {
            app.venc = None;
            worker::reset();
        }

        if last_heartbeat.elapsed() >= HEARTBEAT {
            last_heartbeat = Instant::now();
            let phase = match app.phase {
                Phase::Idle => "idle",
                Phase::Running => "running",
                Phase::Halted => "halted",
                Phase::Error => "error",
            };
            // Guest MIPS is the number worth watching over time: it moves with
            // what else the loop is doing (scanning the framebuffer, encoding
            // video), and a fall to zero on a "running" machine is the shape
            // of a wedged guest.
            let secs = app.boot_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
            let mips = match secs > 0.0 {
                true => app.instret as f64 / 1e6 / secs,
                false => 0.0,
            };
            let idle = app.emu.as_ref().map_or(false, |e| e.get_cpu().is_idle());
            eprintln!(
                "[risc-box] heartbeat: phase={phase} up={secs:.0}s instret={:.2}G mips={mips:.1} \
                 guest_idle={idle} console={}KiB watchers={}/{}{}",
                app.instret as f64 / 1e9,
                app.console_total / 1024,
                server.sse_count("display"),
                server.sse_count("video"),
                app.error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default(),
            );
        }

        // Get responses (the 202 for /start, errors, etc.) onto the wire
        // BEFORE any blocking S3 work in do_start, so the browser isn't left
        // hanging on the fetch.
        server.flush();

        // A failed boot fetch left a scheduled retry: re-queue it when due.
        if app.phase == Phase::Error && app.pending.is_none() {
            if let Some(t) = app.retry_at {
                if Instant::now() >= t {
                    app.retry_at = None;
                    app.retry += 1;
                    app.pending =
                        Some(app.retry_start.take().unwrap_or(Start { creds: None, reset: false }));
                }
            }
        }

        if let Some(start) = app.pending.take() {
            app.phase = Phase::Running; // optimistic; do_start flips to Error on failure
            do_start(&mut app, start);
        }

        let mut busy = false;
        if app.phase == Phase::Running {
            if let Some(emu) = app.emu.as_mut() {
                let parked = app.input_boost == 0 && emu.get_cpu().is_idle();
                let batch = match parked {
                    true => IDLE_BATCH,
                    false => TICK_BATCH,
                };
                // A guest that is RUNNING has already paced this turn: it just
                // spent a full batch of real work, and the loop must not add a
                // sleep on top. `busy` used to be set only by console bytes and
                // encoded frames, so a compute-bound guest that prints nothing
                // — a game, a build, a long boot phase — was silently throttled
                // by a millisecond every turn. At the ~6 ms a batch takes that
                // is a quarter of the machine, given away for nothing.
                busy = !parked;
                app.input_boost = app.input_boost.saturating_sub(1);
                // batched entry point: per-instruction loop overhead is
                // amortized inside the emulator, and a WFI-parked guest
                // consumes the batch without spinning (idle turns cost the
                // loop almost nothing, leaving the budget to scan/encode).
                emu.run_n(batch);
                app.instret += batch;
                // Presented frames per real second. Written out here rather
                // than behind a method because `emu` holds a borrow of app.emu
                // for the rest of this block, and these are disjoint fields.
                {
                    let bytes = emu.fb_bytes();
                    let now = Instant::now();
                    let dt = now.duration_since(app.fps_at).as_secs_f64();
                    if dt >= 1.0 {
                        let per = (display::fb_bytes() as f64).max(1.0);
                        app.fps_now = bytes.wrapping_sub(app.fps_bytes) as f64 / per / dt;
                        app.fps_bytes = bytes;
                        app.fps_at = now;
                        let sdt = now.duration_since(app.sent_at).as_secs_f64();
                        app.sent_fps = (app.sent_frames - app.sent_mark) as f64 / sdt;
                        app.sent_mark = app.sent_frames;
                        app.video_fps = (app.video_frames - app.video_mark) as f64 / sdt;
                        app.video_mark = app.video_frames;
                        app.sent_at = now;
                    }
                }
                // drain the guest UART output into scrollback + SSE
                let mut chunk: Vec<u8> = Vec::new();
                let t = emu.get_mut_terminal();
                loop {
                    let b = t.get_output();
                    if b == 0 {
                        break;
                    }
                    chunk.push(b);
                    if chunk.len() >= 64 * 1024 {
                        break; // bound one drain; more comes next turn
                    }
                }
                if !chunk.is_empty() {
                    app.console_total += chunk.len() as u64;
                    for &b in &chunk {
                        if app.scrollback.len() >= SCROLLBACK {
                            app.scrollback.pop_front();
                        }
                        app.scrollback.push_back(b);
                    }
                    server.broadcast("console", &format!("data: {}", b64(&chunk)));
                    busy = true;
                }
                // exchange ethernet frames between the guest NIC and the
                // user-mode network; traffic in flight lifts the WFI throttle
                // so forwarded connections stay snappy. The boost outlives the
                // frames by ~0.5s of guest CPU: interactive protocols (ping's
                // 1s cadence, TCP handshakes) sleep between packets, and
                // dropping straight back to the idle batch would stretch
                // guest time ~7x mid-conversation.
                if let Some(stack) = app.net.as_mut() {
                    let backend = emu.get_mut_cpu().get_mut_mmu().get_mut_net().get_mut_backend();
                    if stack.pump(backend.as_mut()) {
                        app.input_boost = app.input_boost.max(NET_BOOST_TURNS);
                        busy = true;
                    }
                }
                // display scanout: only while someone is actually watching
                // (an unwatched machine costs zero scan work). Dirty bands go
                // out as deflated SSE events; the browser blits them onto its
                // canvas (see display.rs).
                //
                // Paced by what a scan COSTS rather than by a fixed clock, the
                // same way the AV1 path below is. A flat 100 ms capped the
                // picture at 10 fps whatever the machine was doing — which is
                // both too slow for an idle guest, where a scan is a couple of
                // milliseconds and the thread is free, and too eager for a
                // busy one, where every scan is emulator time the desktop
                // wanted. Spending at most 1/(1+ratio) of the thread lets the
                // frame rate rise on a quiet machine and fall on a working
                // one, which is the right way round for both.
                // A cost budget alone would make a STILL screen more expensive
                // than the old fixed clock did: finding nothing is cheap, so
                // the budget would happily look for nothing sixty times a
                // second. So back off toward the old cadence while the picture
                // is not moving, and snap back to the floor the moment it is.
                // A motion's first frame can then be up to FB_SCAN_MS late —
                // exactly as late as it always was — and every frame after it
                // arrives at the fast rate.
                //
                // With a WORKER (shared-everything-threads), none of the above
                // applies to the expensive part: the guest's thread does one
                // memcpy of the framebuffer and hands it over, and the
                // hashing, diffing, deflating and AV1 all happen on another
                // core. The pacing stays, but it is now pacing a memcpy rather
                // than a compressor, so it barely bites — and one frame is
                // kept in flight, because capturing faster than the worker can
                // compress would only queue up stale screens.
                // Pace to the SLOWEST watcher as well as to the cost of a
                // scan. A frame is only worth producing if the last one has
                // mostly reached someone: past this backlog the extra frames
                // are not seen, they queue — and the queue ends at MAX_WBUF,
                // where the server closes the connection and the viewer loses
                // the stream entirely. On a loopback this never triggers; over
                // a relay, at 1024x768, it triggered within a second.
                const SSE_BACKLOG_LIMIT: usize = 192 * 1024;
                let display_backed_up = server.sse_backlog("display") > SSE_BACKLOG_LIMIT;
                let video_backed_up = server.sse_backlog("video") > SSE_BACKLOG_LIMIT;
                let pull_watching =
                    app.pull_seen.map_or(false, |t| t.elapsed() < Duration::from_secs(3));
                let watching_display =
                    (server.sse_count("display") > 0 && !display_backed_up) || pull_watching;
                let watching_video = server.sse_count("video") > 0 && !video_backed_up;
                // Keep the display scan at its fast floor while input is recent
                // (the same boost window the CPU uses). The stillness backoff
                // stretches the scan interval toward 100 ms when the screen has
                // been quiet, which is right for an idle machine but wrong right
                // after a keystroke: the character lands during a backed-off
                // interval and is not scanned out for up to a full one. Measured
                // ~130 ms input->pixel after a pause vs ~30 ms during continuous
                // input — and the reason typing felt laggier than the mouse,
                // which keeps the scan awake simply by moving. Both the worker
                // and inline scans below pace off this.
                // …but only a QUIET screen needs snapping awake. When the
                // frame is already animating (fb_still == 0 on its own), the
                // floor-paced scan carries every change anyway, and halving
                // the floor just doubles scan+deflate work exactly while the
                // player is providing input — measured as "DOOM lags out when
                // I move the mouse or type", the encode stealing the emulator
                // cycles the game needed. Boost from stillness, never from
                // motion.
                let snap = app.input_boost > 0 && app.fb_still > 0;
                // A video watcher needs frames at the encoder's cadence even
                // when the band diff has nothing to say: bands are not even
                // computed for it, so "still" is structurally true and the
                // backoff otherwise parks a live stream at the 100 ms ceiling
                // (measured: a video-only watcher got exactly 10 fps).
                let scan_still = if snap || watching_video { 0 } else { app.fb_still };
                if worker::available() {
                    let due = app.fb_scanned.map_or(true, |t| {
                        t.elapsed() >= display::scan_interval_boosted(
                            app.fb_cost, scan_still, snap)
                    });
                    if (watching_display || watching_video) && due && worker::inflight() == 0 {
                        let began = Instant::now();
                        let mut buf = worker::take_buffer();
                        Display::capture(emu, &mut buf);
                        worker::submit(worker::Job {
                            frame: buf,
                            want_bands: watching_display,
                            want_video: watching_video,
                        });
                        // The capture is the whole of what the guest now pays.
                        app.fb_cost = (app.fb_cost + began.elapsed()) / 2;
                        app.fb_scanned = Some(Instant::now());
                        busy = true;
                    }
                } else if watching_display
                    && app.fb_scanned.map_or(true, |t| {
                        t.elapsed() >= display::scan_interval_boosted(
                            app.fb_cost, scan_still, snap)
                    })
                {
                    let began = Instant::now();
                    let bands = app.display.scan(emu);
                    app.fb_still = match bands.is_empty() {
                        true => app.fb_still.saturating_add(1),
                        false => 0,
                    };
                    app.sent_frames += !bands.is_empty() as u64;
                    for band in bands {
                        let full = band.x == 0 && band.w == display::fb_w()
                            && band.y == 0 && band.h == display::fb_h();
                        let ev = format!(
                            "{{\"x\":{},\"w\":{},\"y\":{},\"h\":{},\"b\":\"{}\"}}",
                            band.x, band.w, band.y, band.h, b64(&band.z)
                        );
                        app.pull.push(&ev, full);
                        server.broadcast("display", &format!("data: {ev}"));
                        busy = true;
                    }
                    // Smoothed, so one expensive full-frame band (a new
                    // watcher, or a whole-screen repaint) does not stall the
                    // stream for a second afterwards.
                    app.fb_cost = (app.fb_cost + began.elapsed()) / 2;
                    app.fb_scanned = Some(Instant::now());
                }
                // AV1 video scan: same watch-gating + pacing as the display,
                // but capture -> rav1e encode -> base64 SSE. A fresh viewing
                // session (encoder is None) gets a new encoder so its first
                // frame is a keyframe; when nobody watches, the encoder is
                // dropped so the next session starts clean.
                if watching_video && !worker::available() {
                    let packed = worker::packed_params();
                    if app.venc.as_ref().map(|(p, _)| *p) != Some(packed) {
                        app.venc = worker::build_encoder();
                    }
                    // Pace the encoder by what it COSTS, not by the clock.
                    //
                    // Encoding AV1 in here is not free work on an idle thread —
                    // it is the same thread the guest runs on, so every frame
                    // is emulator time the machine did not get. Measured on a
                    // pinned workload, a fixed 10 fps cadence took 82% of the
                    // guest's speed: 36 MIPS with nobody watching, 6.6 MIPS
                    // with the AV1 stream attached. A desktop that starts in
                    // four minutes takes twenty while you watch it start.
                    //
                    // So: after each frame, wait until at least
                    // VIDEO_COST_RATIO times as long has been spent NOT
                    // encoding. The stream slows down on a machine that is
                    // working hard and speeds up on one that is idle, which is
                    // the right way round — and the guest keeps the large
                    // majority of the thread either way.
                    const VIDEO_COST_RATIO: u32 = 4; // encode ≤ 1/(1+4) of the time
                    let due = app.video_scanned.map_or(true, |t| {
                        let floor = std::time::Duration::from_millis(display::FB_SCAN_FLOOR_MS);
                        t.elapsed() >= floor.max(app.video_cost * VIDEO_COST_RATIO)
                            && t.elapsed() >= worker::VIDEO_MIN_INTERVAL
                    });
                    if due {
                        let began = Instant::now();
                        if worker::take_force_key() {
                            if let Some((_, enc)) = app.venc.as_mut() {
                                enc.force_keyframe();
                            }
                        }
                        let mut fresh = vec![0u8; display::fb_bytes()];
                        emu.read_physical_range(display::FB_BASE, &mut fresh);
                        if let Some((_, enc)) = app.venc.as_mut() {
                            let frames = enc.encode_capture(&fresh).unwrap_or_else(|| {
                                let (rgb, w, h) = video::rgb_from_capture(&fresh);
                                enc.encode(&rgb, w, h)
                            });
                            for f in frames {
                                server.broadcast(
                                    "video",
                                    &format!("data: {{\"k\":{},\"d\":\"{}\"}}", f.keyframe as u8, b64(&f.data)),
                                );
                                app.video_frames += 1;
                                busy = true;
                            }
                        }
                        // What this frame actually cost, smoothed so one slow
                        // keyframe does not stall the stream for seconds.
                        app.video_cost = (app.video_cost + began.elapsed()) / 2;
                        app.video_scanned = Some(Instant::now());
                    }
                } else if app.venc.is_some() && !worker::available() {
                    app.venc = None;
                }
            }
        }

        // Collect whatever the worker finished and put it on the wire. This is
        // out here, not inside the emulator block, because a client watching a
        // machine that just stopped should still receive the last frame the
        // worker was holding — and because broadcasting is the one thing the
        // worker is forbidden to do: SET gives it its own fd namespace, so a
        // socket opened here is EBADF over there.
        while let Some(out) = worker::collect() {
            app.fb_still = match out.bands.is_empty() {
                true => app.fb_still.saturating_add(1),
                false => 0,
            };
            app.sent_frames += !out.bands.is_empty() as u64;
            for band in out.bands {
                let full = band.x == 0 && band.w == display::fb_w()
                    && band.y == 0 && band.h == display::fb_h();
                let ev = format!("{{\"x\":{},\"w\":{},\"y\":{},\"h\":{},\"b\":\"{}\"}}",
                                 band.x, band.w, band.y, band.h, b64(&band.z));
                app.pull.push(&ev, full);
                server.broadcast("display", &format!("data: {ev}"));
                busy = true;
            }
            for f in out.video {
                server.broadcast(
                    "video",
                    &format!("data: {{\"k\":{},\"d\":\"{}\"}}", f.keyframe as u8, b64(&f.data)),
                );
                app.video_frames += 1;
                busy = true;
            }
            // Reported, not budgeted: with a worker this time is not the
            // guest's, which is the whole reason for the worker.
            app.video_cost = (app.video_cost + out.cost) / 2;
            worker::recycle(out.spare);
        }

        // Long-poll release: answer parked /fb.bands waiters the moment the
        // ring has moved past their `since`, or empty at their deadline so
        // the connection never sits silent long enough for anything between
        // here and the client to declare it dead.
        if !app.pull_waiters.is_empty() {
            let now = Instant::now();
            let waiters = std::mem::take(&mut app.pull_waiters);
            for (ticket, since, deadline) in waiters {
                if app.pull.gen > since || now >= deadline {
                    let body = fb_bands_body(&mut app, since);
                    let ok = server.release(
                        ticket,
                        Response::new(200, "OK")
                            .with("cache-control", "no-store")
                            .body("application/json", body.into_bytes()),
                    );
                    if ok {
                        busy = true;
                    }
                } else {
                    app.pull_waiters.push((ticket, since, deadline));
                }
            }
        }

        let flushed = server.flush();
        // Running with real CPU work paces the loop; only sleep when idle or
        // when a running machine produced no output and moved no bytes.
        if app.phase != Phase::Running {
            std::thread::sleep(std::time::Duration::from_millis(20));
        } else if !busy && !flushed {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
