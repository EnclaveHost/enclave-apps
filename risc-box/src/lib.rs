//! risc-box — run real machines on the enclave's CPU, booted from OS images
//! in an S3 bucket, with their serial consoles bridged to your browser.
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
//! process holding the machines in RAM, HTTP served on the loopback `http:`
//! port the enclave's TLS proxy forwards to (see network-test / the suite's
//! httpd.rs). The single thread interleaves CPU batches with HTTP polling.
//!
//! One process, many machines. The `main` machine is the one the config
//! describes (its images, its RAM, its port forwards, its streams). Beside it
//! the app hosts INSTANCES: machines forked from a snapshot — the config's
//! root snapshot, any snapshot object, or the live `main` machine itself —
//! each with its own RAM, disk overlay, console and network. Guest RAM is
//! chunked and copy-on-write and the base disk is shared, so N instances of
//! one booted image cost one image plus what each has written since; that is
//! what fits a room full of 512 MiB guests into one 4 GiB wasm32 process.
//! A round-robin scheduler shares the core between whichever machines are
//! not parked in WFI. Instances are addressed as `/i/<id>/<route>`.
//!
//! The guest also gets a virtio-net NIC terminated in user space by src/net.rs
//! (smoltcp): a DHCP server leases 10.0.2.15, and raw `tcp:` deployment ports
//! are spliced onto guest TCP connections (default tcp:2222 -> guest 22, so
//! `ssh -p 2222` reaches an sshd inside the machine). Outbound, the gateway
//! NATs guest flows onto real sockets slirp-style (TCP splices, per-flow UDP,
//! a DNS proxy at 10.0.2.2, gateway-answered ICMP echo), so `ping 8.8.8.8`
//! and `curl` work from the guest shell; `net.outbound: false` seals it.
//! Instances get the same NAT; the inbound forwards belong to `main`.
//!
//! Routes (bare = the main machine; `/i/<id>/...` = an instance):
//!   GET  /            console UI (self-contained HTML + embedded xterm)
//!   GET  /a/<asset>   embedded xterm.js / xterm.css
//!   GET  /status      JSON machine state (phase, image sizes, instret, MIPS)
//!   POST /start       {accessKeyId?,secretAccessKey?,sessionToken?,reset?,snapshot?}
//!                     fetch images from S3 (creds: body > config > unsigned)
//!                     and boot — or resume from the snapshot when one is
//!                     cached; reset:true re-fetches instead of using cache;
//!                     snapshot:false forces a cold boot. On an instance:
//!                     re-fork from its origin.
//!   POST /input       raw bytes → the guest UART receive register
//!   POST /exec        {cmd,timeout_s?,max_bytes?} run a shell command on the
//!                     guest console and return its stdout + exit code (JSON)
//!   GET  /console     Server-Sent Events: base64 console output, scrollback first
//!   POST /save        dump the (guest-modified) disk and PUT it to saveKey (main)
//!   POST /snapshot    {key?,level?} serialize the RUNNING machine (CPU,
//!                     devices, RAM, disk delta) and PUT it to the snapshot
//!                     key; later starts resume from it instead of booting
//!   POST /stop        halt the machine and drop it from RAM
//!   GET  /instances   the machines this process hosts
//!   POST /instances   {from?:"main"|<snapshot key>, id?} fork a new instance
//!   DELETE /i/<id>    stop and forget an instance
//!   GET  /display     Server-Sent Events: the main machine's screen as
//!                     deflated dirty bands (see display.rs)
//!   GET  /fb.png      the current frame as one PNG snapshot (any machine)
//!   GET  /ping        liveness

mod display;
mod egress;
mod gamestream;
mod gz;
mod httpd;
mod net;
// Hardware H.264 on the fleet GPU, reached through wasi-nn (PLATFORM.md).
mod nvenc;
mod opl;
mod s3;
mod video;
mod worker;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use display::Display;
use httpd::{form_get, json, Request, Response, Server};
use net::{ForwardCfg, HostNet, NetStack};
use riscv_emu_rust::terminal::Terminal;
use riscv_emu_rust::{Emulator, SnapshotImage, SnapshotInfo};
use s3::{Creds, Endpoint};
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

static INDEX_HTML: &str = include_str!("index.html");
static XTERM_JS: &str = include_str!("vendor/xterm.js");
static XTERM_CSS: &str = include_str!("vendor/xterm.css");

const DEFAULT_PORT: u16 = 8000; // fleet policy: http:8000, never 8080
const MAX_BODY: usize = 256 * 1024;
const TICK_BATCH: u64 = 400_000; // CPU instructions per event-loop turn
const IDLE_BATCH: u64 = 4_000; // batch while the guest is parked in WFI: keeps
                               // timers/devices ticking at ~1% of the busy rate
                               // so an idle machine stops burning the host CPU
/// With several machines busy at once the turn's instruction budget is split
/// between them, so a turn stays about as long as it was with one; this is
/// the floor one machine's share may sink to, so a crowded box still makes
/// progress on every guest each turn.
const MIN_BATCH: u64 = 50_000;
const SCROLLBACK: usize = 256 * 1024; // console bytes retained for late joiners
// Full-speed turns after network activity: ~100M instructions ≈ 1.25 guest
// seconds, enough to span a whole ping/keepalive cadence so an interactive
// network session never drops into the ~20x-slow idle clock mid-conversation.
const NET_BOOST_TURNS: u64 = 250;

/// Audio is pushed once this much has accumulated, and never more than the
/// larger figure at a time. ~12 ms and ~93 ms at 11 kHz stereo. The minimum
/// sets how lumpy delivery is, and the listener has to buffer for the lumps —
/// so it is deliberately smaller than one turn's worth of audio, which puts
/// the real cadence at the loop's own rate rather than at a threshold.
const AUDIO_MIN_CHUNK: usize = 512;

/// Drain generously: the point is to leave the card's ring empty, so a
/// backlog from a slow turn clears in one or two turns instead of lingering
/// as delay (or overflowing into drops).
const AUDIO_MAX_CHUNK: usize = 16384;

/// Turns of full-batch running granted after real HID input lands.
///
/// While `input_boost` is non-zero the loop is never `parked`, so every turn
/// runs a whole TICK_BATCH and the turn never sleeps. That is the right trade
/// for one keystroke and a disaster when it is held down: the main loop then
/// owns the tenant's CPU continuously and the display worker gets none, so
/// video PRODUCTION collapses while every individual turn still looks healthy.
///
/// Measured on metal0 (0.25 CPU share, guest ~122 MIPS, DOOM running): with
/// NET_BOOST_TURNS (250) used here, 10 accepted POST /hid per second drove
/// videoFps 40 -> 0 and left 70% of the client's 100 ms windows empty, with
/// turnMax still 8 ms. The same request REJECTED at parse (which never reaches
/// this line) stayed at 40 fps, as did a 1-byte POST /input at the same 10/s —
/// and that one is a 3-turn boost. What hurts is the duty cycle, not the
/// request rate, so the boost only has to outlast the guest's IRQ-and-repaint,
/// not a whole second of it.
const INPUT_BOOST_TURNS: u64 = 32;

/// Longest unbroken run of boosted turns, and the cooldown that follows it.
///
/// The cap is what makes the bound hold at ANY input rate rather than just the
/// rates a well-behaved client happens to send: without it, input arriving
/// faster than INPUT_BOOST_TURNS decays simply re-arms the boost forever.
/// Measured: with the boost merely shortened, real cursor moves were clean to
/// 20/s (the bridge's ceiling is ~20/s) but cost 23% of client frames again at
/// 40/s. RUN_MAX above INPUT_BOOST_TURNS so an isolated event is never cut
/// short; the hold is the window the display worker is guaranteed.
const BOOST_RUN_MAX: u64 = 48;
const BOOST_HOLD_TURNS: u64 = 48;

/// The one machine that is never an instance.
const MAIN_ID: &str = "main";

/// The largest guest RAM a machine may be configured with. On a 32-bit wasm
/// build the whole process has a 4 GiB address space and everything else
/// (the base disk, images, the other machines) lives in it too, so a single
/// guest stops at 1920 MiB. On a memory64 build the process can address far
/// more, and the deployment's RAM slice (`-W max-memory-size`) is the real
/// limit; 64 GiB here is a sanity cap, not a promise.
#[cfg(target_pointer_width = "64")]
const RAM_MIB_MAX: u64 = 64 * 1024;
/// Default host-memory budget for all machines together: 3 GiB inside a
/// 4 GiB wasm32 process; 32 GiB on memory64, where the deployment's RAM
/// slice is what actually bounds it (`instances.maxBytes` overrides).
#[cfg(target_pointer_width = "64")]
const INSTANCES_MAX_BYTES_DEFAULT: u64 = 32 << 30;
#[cfg(not(target_pointer_width = "64"))]
const INSTANCES_MAX_BYTES_DEFAULT: u64 = 3 << 30;
#[cfg(not(target_pointer_width = "64"))]
const RAM_MIB_MAX: u64 = 1920;

// ---- config ---------------------------------------------------------------

pub struct Config {
    pub title: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    kernel: String,
    fs: String,
    dtb: Option<String>,
    save_key: Option<String>,
    pub config_creds: Option<Creds>,
    autostart: bool,
    read_only: bool,
    net_enabled: bool,
    net_outbound: bool,
    forwards: Vec<ForwardCfg>,
    // Guest RAM in MiB (`ramMiB`) for the main machine. Default 512 keeps
    // existing deployments' footprint; the alpine/firefox image wants 1792.
    // Clamped to [128, RAM_MIB_MAX] (1920 on wasm32, where the address space
    // is 4 GiB; far higher on a memory64 build), and a machine under 128 MiB
    // can't even finish X startup. Instances take theirs from the snapshot
    // they fork from.
    ram_mib: u64,
    // `"ramMiB": "auto"`: size the guest to the deployment's whole memory
    // slice instead of a number. Resolved once the images are in hand (the
    // disk is the biggest thing sharing the address space), against the
    // ceiling the platform hands the guest in ENCLAVE_MEM_MB. `ram_mib`
    // above stays the fallback for a host that does not say.
    ram_auto: bool,
    // Was `instances.maxBytes` written down? An auto-sized machine derives
    // the whole-box budget from the slice too, but an explicit number wins.
    instances_max_bytes_set: bool,
    // Display size (`display: {width, height}`), default 1024x768. Must fit the
    // DTB's 8 MiB framebuffer window; the emulator applies the same guard to
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
    // Instant boot. `snapshot` names the S3 key a snapshot of the booted
    // machine lives under: when the object exists, /start resumes from it —
    // seconds — instead of running the guest's own boot, which on an emulated
    // core is minutes. When it does not exist yet the boot is a cold one and
    // POST /snapshot writes the object (to `snapshotSaveKey`, defaulting to
    // `snapshot`) so every start after that is instant. `snapshotLevel` is
    // the deflate level the RAM image is written at (1 fast .. 9 small).
    // The same object is the default ROOT that instances fork from.
    snapshot: Option<String>,
    snapshot_save_key: Option<String>,
    snapshot_level: u8,
    // A shell command run on the guest console right after a restore or a
    // fork, via the /exec machinery. A resumed guest still believes it is the
    // moment the snapshot was taken: its wall clock is stale and its random
    // pool is the same one every other resume of this snapshot has. `{epoch}`
    // expands to the host's UNIX time and `{entropy}` to 64 fresh random
    // bytes as hex, so `date -s @{epoch}; echo {entropy} > /dev/urandom`
    // fixes both.
    restore_exec: Option<String>,
    restore_exec_timeout_s: u64,
    // `instances: {max, maxBytes}`: how many machines this process may host
    // (main included) and the host-memory budget they may add up to. The
    // budget is the honest limit: it counts what machines have actually
    // touched (owned RAM chunks, disk overlay) plus the shared images and
    // the base disk, against a wasm32 address space that ends at 4 GiB.
    instances_max: usize,
    instances_max_bytes: u64,
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
    let inst = v.get("instances");
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
            .clamp(128, RAM_MIB_MAX),
        // the string form, `"ramMiB": "auto"` (a number keeps its meaning)
        ram_auto: v
            .get("ramMiB")
            .and_then(|x| x.as_str())
            .map(|t| t.trim().eq_ignore_ascii_case("auto"))
            .unwrap_or(false),
        instances_max_bytes_set: inst.and_then(|i| i.get("maxBytes")).is_some(),
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
        snapshot: s("snapshot"),
        snapshot_save_key: s("snapshotSaveKey").or_else(|| s("snapshot")),
        snapshot_level: v
            .get("snapshotLevel")
            .and_then(|x| x.as_u64())
            .unwrap_or(2)
            .clamp(1, 9) as u8,
        restore_exec: s("restoreExec"),
        restore_exec_timeout_s: v
            .get("restoreExecTimeoutS")
            .and_then(|x| x.as_u64())
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_S)
            .clamp(1, EXEC_MAX_TIMEOUT_S),
        instances_max: inst
            .and_then(|i| i.get("max"))
            .and_then(|x| x.as_u64())
            .unwrap_or(8)
            .clamp(1, 64) as usize,
        instances_max_bytes: inst
            .and_then(|i| i.get("maxBytes"))
            .and_then(|x| x.as_u64())
            .unwrap_or(INSTANCES_MAX_BYTES_DEFAULT),
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

fn new_terminal() -> Box<RiscBoxTerminal> {
    Box::new(RiscBoxTerminal { input: VecDeque::new(), output: VecDeque::new() })
}

// ---- app state -------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Halted,
    Error,
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Idle => "idle",
        Phase::Running => "running",
        Phase::Halted => "halted",
        Phase::Error => "error",
    }
}

struct Images {
    kernel: Vec<u8>,
    /// The root filesystem, expanded, shared by every machine that boots
    /// from it (each keeps its own overlay of the blocks it writes). Held for
    /// the lifetime of the app: a restart or a new instance needs no
    /// download and no inflate. The compressed form is not kept — the
    /// expanded one is what every start needs, and the pair together were
    /// more than a second machine's worth of address space.
    disk: Arc<Vec<u8>>,
    fs_bytes: usize, // as fetched (gzipped when the key says so)
    fs_gzipped: bool,
    dtb: Option<Vec<u8>>,
    /// sha256 (hex) of the kernel and fs objects exactly as the bucket served
    /// them: the part of a snapshot's identity that says which images it was
    /// taken against (see `identity`).
    kernel_sha256: String,
    fs_sha256: String,
    /// The machine snapshot under the configured `snapshot` key, when the
    /// object existed at fetch time or /snapshot has written one since. Held
    /// exactly like the images, so a restart resumes without a download.
    snap_stored: Option<Vec<u8>>,
}

impl Images {
    /// What a snapshot is bound to. The delta inside a snapshot is relative
    /// to the base disk it was taken from, and the RAM inside it embodies the
    /// kernel, the device tree and the display geometry — so a restore must
    /// present exactly these, and refuses (cold-booting instead) otherwise.
    /// RAM size is NOT here: it is carried by the snapshot itself and
    /// checked by the emulator, so a 512 MiB root can serve instances beside
    /// a 1792 MiB main machine.
    fn identity(&self, cfg: &Config) -> String {
        format!(
            "kernel:{} fs:{} dtb:{} fb:{}x{} realtime:{}",
            self.kernel_sha256,
            self.fs_sha256,
            self.dtb.as_ref().map(|d| sha256_hex(d)).unwrap_or_else(|| "-".into()),
            cfg.fb_w,
            cfg.fb_h,
            cfg.realtime
        )
    }
}

struct Start {
    creds: Option<Creds>,
    reset: bool,
    /// None = resume from the snapshot if one is cached; Some(false) forces a
    /// cold boot of the base images; Some(true) is the same as None.
    snapshot: Option<bool>,
}

/// One machine: the emulator and everything the app keeps per guest. The
/// streams (display, video, audio, GameStream) are the MAIN machine's and
/// live on `App`; every machine has a console, a NIC, /exec and /hid.
struct Machine {
    id: String,
    /// Where it came from: "config" (booted or restored from the config's
    /// images), "main" (forked from the live main machine) or a snapshot key.
    origin: String,
    emu: Option<Emulator>,
    /// The image an instance was forked from, kept so /start can re-fork it
    /// and so its pages stay shared for as long as the instance lives.
    image: Option<Arc<SnapshotImage>>,
    image_key: Option<String>,
    phase: Phase,
    error: Option<String>,
    ram_mib: u64,
    created: Instant,
    boot_at: Option<Instant>,
    instret: u64,
    // Presented frames per real second, sampled from the framebuffer's byte
    // counter (see the fps sampling in the loop).
    fps_now: f64,
    fps_bytes: u64,
    fps_at: Instant,
    input_boost: u64, // turns to force full tick batches after POST /input
    /// Consecutive turns the boost has been held, and the cooldown that follows
    /// when it has been held too long. Input arriving faster than the boost
    /// decays would otherwise re-arm it forever; see BOOST_RUN_MAX.
    boost_run: u64,
    boost_hold: u64,
    exec_seq: u64, // per-command nonce for /exec console markers
    scrollback: VecDeque<u8>,
    console_total: u64,
    net: Option<NetStack>, // this machine's user-mode network
    // How the machine came up: resumed from a snapshot (and how long the
    // restore took, and when that snapshot was taken) or booted cold.
    restored: bool,
    restore_ms: f64,
    restore_taken_unix: u64,
    last_snapshot: Option<String>,
    last_save: Option<String>,
}

impl Machine {
    fn new(id: &str, origin: &str) -> Self {
        Machine {
            id: id.to_string(),
            origin: origin.to_string(),
            emu: None,
            image: None,
            image_key: None,
            phase: Phase::Idle,
            error: None,
            ram_mib: 0,
            created: Instant::now(),
            boot_at: None,
            instret: 0,
            fps_now: 0.0,
            fps_bytes: 0,
            fps_at: Instant::now(),
            input_boost: 0,
            boost_run: 0,
            boost_hold: 0,
            exec_seq: 0,
            scrollback: VecDeque::new(),
            console_total: 0,
            net: None,
            restored: false,
            restore_ms: 0.0,
            restore_taken_unix: 0,
            last_snapshot: None,
            last_save: None,
        }
    }

    fn is_main(&self) -> bool {
        self.id == MAIN_ID
    }

    /// The SSE topic this machine's console broadcasts on.
    fn console_topic(&self) -> String {
        match self.is_main() {
            true => "console".to_string(),
            false => format!("console:{}", self.id),
        }
    }

    /// Frames the guest has presented per real second, over the last sampling
    /// window. A frame is `width * height * 4` bytes painted into the
    /// framebuffer, so this counts what the machine actually put on screen,
    /// not what it claims: the guest's own timing is only as honest as the
    /// guest's clock, and unless `realtime` is set that clock runs at
    /// (MIPS / 10) times speed.
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

    /// Host bytes this machine holds itself: owned RAM chunks and disk
    /// overlay. Shared pages are counted once, on the image they belong to.
    fn footprint(&self) -> usize {
        match self.emu.as_ref() {
            Some(e) => {
                let f = e.footprint();
                f.ram_owned + f.disk_overlay
            }
            None => 0,
        }
    }

    /// Halt: drop the emulator (and with it the owned pages). The record,
    /// the console scrollback and the origin stay, so /start can bring it
    /// back and /status can say what happened.
    fn halt(&mut self) {
        self.emu = None;
        self.phase = Phase::Halted;
        self.boot_at = None;
        self.restored = false;
    }

    fn running(&self) -> bool {
        self.phase == Phase::Running && self.emu.is_some()
    }

    fn summary_json(&self) -> String {
        let js = |v: &Option<String>| {
            v.as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into())
        };
        format!(
            "{{\"id\":\"{}\",\"origin\":\"{}\",\"phase\":\"{}\",\"ramMiB\":{},\"ageSecs\":{},\"upSecs\":{},\"instret\":{},\"mips\":{:.1},\"restored\":{},\"consoleBytes\":{},\"footprintBytes\":{},\"error\":{}}}",
            httpd::json_escape(&self.id),
            httpd::json_escape(&self.origin),
            phase_name(self.phase),
            self.ram_mib,
            self.created.elapsed().as_secs(),
            self.boot_at.map_or(0, |t| t.elapsed().as_secs()),
            self.instret,
            self.mips(),
            self.restored,
            self.console_total,
            self.footprint(),
            js(&self.error),
        )
    }
}

struct App {
    cfg: Config,
    /// Index 0 is always the main machine.
    machines: Vec<Machine>,
    /// Snapshot images machines fork from, by key ("main@<unix>" for images
    /// taken from the live main machine, the S3 key otherwise). A bucket root
    /// stays resident once loaded — it IS the root every new instance wants;
    /// an image taken from main is dropped with its last instance.
    root_images: HashMap<String, Arc<SnapshotImage>>,
    /// The music synth. Host-side on purpose; see src/opl.rs.
    opl: opl::Opl,
    pending: Option<Start>,
    cache: Option<Images>,
    live_creds: Option<Creds>, // remembered from the last successful start, for /save
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
    // Worst main-loop turn since the last /status read, with its per-phase
    // breakdown. The loop is the app: a slow turn is every client's stall at
    // once, and until this existed those stalls were misattributed to the
    // platform (a warm probe against the box's own control plane finally
    // separated the layers).
    turn_max_ms: f64,
    turn_max_detail: String,
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
    /// The in-guest GameStream host. Built once the machine is running (it
    /// needs an RSA identity, which costs seconds, and there is nothing to
    /// stream before then). `None` when the ports could not be bound -- the
    /// app still serves its own HTTP surface in that case.
    gs: Option<gamestream::host::Host>,
    /// Set once so a failed bind is not retried every turn.
    gs_tried: bool,
    fb_scanned: Option<Instant>, // last display scan (paced by its own cost)
    fb_overlay_frame: Option<u64>, // completed fullscreen frame last submitted
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
    fn main(&self) -> &Machine {
        &self.machines[0]
    }

    fn main_mut(&mut self) -> &mut Machine {
        &mut self.machines[0]
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.machines.iter().position(|m| m.id == id)
    }

    /// Host bytes in use: every machine's own pages, every resident image,
    /// the shared base disk and the cached objects.
    fn footprint(&self) -> usize {
        let machines: usize = self.machines.iter().map(|m| m.footprint()).sum();
        let images: usize = self.root_images.values().map(|i| i.footprint()).sum();
        let cache = self
            .cache
            .as_ref()
            .map(|c| c.disk.len() + c.kernel.len() + c.snap_stored.as_ref().map_or(0, |s| s.len()))
            .unwrap_or(0);
        machines + images + cache
    }

    /// Device-level display counters for chasing capture bugs: which surface
    /// moved, what the scanout holds. Cheap (sums 4 KiB), debug-grade.
    fn gpu_debug_json(&self, mi: usize) -> String {
        match self.machines[mi].emu.as_ref() {
            Some(emu) => {
                let flushes = emu.gpu_flushes();
                let fbb = emu.fb_bytes();
                let mode = emu.gpu_mode();
                // scanout, read directly with prefer_gpu (full-size buffer)
                let mut buf = vec![0u8; display::fb_bytes()];
                let from_gpu = emu.read_display(display::FB_BASE, &mut buf, true);
                let scan_sum: u64 = buf.iter().map(|&b| b as u64).sum();
                // the REAL capture path, arbitration included
                display::Display::capture(emu, &mut buf);
                let cap_sum: u64 = buf.iter().map(|&b| b as u64).sum();
                let sum = if from_gpu { 1u64 } else { 0 };
                format!(
                    "{{\"flushes\":{},\"fbBytes\":{},\"mode\":\"{}\",\"fromGpu\":{},\"scanSum\":{},\"capSum\":{}}}",
                    flushes,
                    fbb,
                    mode.map(|(w, h)| format!("{}x{}", w, h)).unwrap_or_default(),
                    sum,
                    scan_sum,
                    cap_sum
                )
            }
            None => "null".into(),
        }
    }

    fn status_json(&self, mi: usize) -> String {
        let m = &self.machines[mi];
        let img = self
            .cache
            .as_ref()
            .map(|i| {
                format!(
                    ",\"kernelBytes\":{},\"fsBytes\":{},\"fsGzipped\":{}",
                    i.kernel.len(),
                    i.fs_bytes,
                    i.fs_gzipped
                )
            })
            .unwrap_or_default();
        let js = |v: &Option<String>| {
            v.as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into())
        };
        format!(
            "{{\"phase\":\"{}\",\"id\":\"{}\",\"origin\":\"{}\",\"title\":\"{}\",\"endpoint\":\"{}\",\"bucket\":\"{}\",\
             \"kernel\":\"{}\",\"fs\":\"{}\",\"saveKey\":{},\"readOnly\":{},\
             \"instret\":{},\"mips\":{:.1},\"fps\":{:.1},\"sentFps\":{:.1},\"videoFps\":{:.1},\"videoMs\":{:.1},\"capMs\":{:.2},\"turnMaxMs\":{:.0},\"turnMax\":\"{}\",\"display\":{{\"width\":{},\"height\":{},\"realtime\":{}}},\
             \"consoleBytes\":{},\"lastSave\":{},\"error\":{},\"net\":{},\"ramMiB\":{},\"cursor\":{},\"gpuDebug\":{},\"snapshot\":{},\"instances\":{}{img}}}",
            phase_name(m.phase),
            httpd::json_escape(&m.id),
            httpd::json_escape(&m.origin),
            httpd::json_escape(&self.cfg.title),
            httpd::json_escape(&self.cfg.endpoint),
            httpd::json_escape(&self.cfg.bucket),
            httpd::json_escape(&self.cfg.kernel),
            httpd::json_escape(&self.cfg.fs),
            js(&self.cfg.save_key),
            self.cfg.read_only,
            m.instret,
            m.mips(),
            m.fps(),
            self.sent_fps,
            self.video_fps,
            self.video_cost.as_secs_f64() * 1000.0,
            self.fb_cost.as_secs_f64() * 1000.0,
            self.turn_max_ms,
            httpd::json_escape(&self.turn_max_detail),
            display::fb_w(),
            display::fb_h(),
            self.cfg.realtime,
            m.console_total,
            js(&m.last_save),
            js(&m.error),
            m.net
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
            m.ram_mib,
            // The pointer as the DEVICE holds it: resource, position, and how
            // many cursor commands have arrived. Screenshots cannot answer
            // "is the guest still moving the cursor" — a frozen pointer looks
            // identical whether the guest stopped sending, the host stopped
            // compositing, or the resource was freed underneath it. Cost is a
            // few bytes on a status poll.
            m.emu
                .as_ref()
                .and_then(|e| e.gpu_cursor())
                .map(|(res, x, y, n)| format!(
                    "{{\"res\":{},\"x\":{},\"y\":{},\"updates\":{}}}", res, x, y, n))
                .unwrap_or_else(|| "null".into()),
            self.gpu_debug_json(mi),
            self.snapshot_json(mi),
            self.instances_summary_json(),
        )
    }

    /// The snapshot facts for one machine: which key, whether one is cached
    /// (and how big), whether the machine was resumed from one, and what the
    /// last /snapshot wrote.
    fn snapshot_json(&self, mi: usize) -> String {
        let m = &self.machines[mi];
        let js = |v: &Option<String>| {
            v.as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into())
        };
        format!(
            "{{\"key\":{},\"cachedBytes\":{},\"restored\":{},\"restoreMs\":{:.0},\"takenUnix\":{},\"lastSnapshot\":{}}}",
            js(&self.cfg.snapshot),
            self.cache
                .as_ref()
                .and_then(|c| c.snap_stored.as_ref())
                .map(|b| b.len())
                .unwrap_or(0),
            m.restored,
            m.restore_ms,
            m.restore_taken_unix,
            js(&m.last_snapshot),
        )
    }

    fn instances_summary_json(&self) -> String {
        format!(
            "{{\"count\":{},\"max\":{},\"footprintBytes\":{},\"maxBytes\":{},\"images\":{}}}",
            self.machines.len(),
            self.cfg.instances_max,
            self.footprint(),
            self.cfg.instances_max_bytes,
            self.root_images.len()
        )
    }

    fn instances_json(&self) -> String {
        let list: Vec<String> = self.machines.iter().map(|m| m.summary_json()).collect();
        let images: Vec<String> = self
            .root_images
            .iter()
            .map(|(k, i)| {
                format!(
                    "{{\"key\":\"{}\",\"ramMiB\":{},\"takenUnix\":{},\"footprintBytes\":{},\"users\":{}}}",
                    httpd::json_escape(k),
                    i.meta.ram_bytes >> 20,
                    i.meta.taken_unix,
                    i.footprint(),
                    Arc::strong_count(i) - 1
                )
            })
            .collect();
        format!(
            "{{\"instances\":[{}],\"images\":[{}],\"summary\":{}}}",
            list.join(","),
            images.join(","),
            self.instances_summary_json()
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
    let kernel_sha256 = sha256_hex(&kernel);
    let fs_sha256 = sha256_hex(&fs_stored);
    let fs_bytes = fs_stored.len();
    // A `.gz` key is expanded here, once, and the expanded disk is what every
    // machine shares from now on. A bad object fails the fetch (which
    // retries) rather than the machine start.
    let fs_gzipped = gz::is_gzip_key(&cfg.fs);
    let disk = match fs_gzipped {
        true => {
            let raw = gz::gunzip(&fs_stored)?;
            eprintln!(
                "[risc-box]   fs {} bytes gzipped -> {} bytes ({:.1}x)",
                fs_bytes,
                raw.len(),
                raw.len() as f64 / fs_bytes.max(1) as f64
            );
            raw
        }
        false => {
            eprintln!("[risc-box]   fs {} bytes", fs_bytes);
            fs_stored
        }
    };
    let dtb = match &cfg.dtb {
        Some(k) => Some(
            s3::get_object(&ep, &cfg.bucket, k, creds, &mut noop)
                .map_err(|e| format!("fetch dtb {k}: {e}"))?,
        ),
        None => None,
    };
    // The snapshot rides the same fetch (and the same retry budget): a
    // transient egress blip at enclave cold boot must not silently turn an
    // instant start into a two-minute one. A missing object is not an error,
    // it is the state before the first /snapshot.
    let snap_stored = match &cfg.snapshot {
        Some(k) => {
            eprintln!("[risc-box] fetching snapshot s3://{}/{k}", cfg.bucket);
            match s3::get_object_opt(&ep, &cfg.bucket, k, creds, &mut progress_logger("snapshot"))
                .map_err(|e| format!("fetch snapshot {k}: {e}"))?
            {
                Some(mut b) => {
                    b.shrink_to_fit();
                    eprintln!("[risc-box]   snapshot {} bytes", b.len());
                    Some(b)
                }
                None => {
                    eprintln!(
                        "[risc-box]   no snapshot at {k} yet: this boot is a cold one (POST /snapshot once the machine is ready)"
                    );
                    None
                }
            }
        }
        None => None,
    };
    Ok(Images {
        kernel,
        disk: Arc::new(disk),
        fs_bytes,
        fs_gzipped,
        dtb,
        kernel_sha256,
        fs_sha256,
        snap_stored,
    })
}

/// A fresh emulator configured the way every machine here is: RAM size,
/// display geometry, clock source. What comes next — a kernel, a snapshot,
/// or an image — is the caller's.
/// What the platform will let this whole process address, in MiB, or None
/// when it did not say (a local `wasmtime run`, or a fleet older than the
/// hint). `-W max-memory-size` is enforced by the engine but invisible from
/// inside: without this a guest can only find its ceiling by dying at it.
fn slice_mib() -> Option<u64> {
    std::env::var("ENCLAVE_MEM_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
}

/// Everything the box holds that is NOT guest RAM, plus room to work in.
///
/// The disk image dominates: it is held expanded and shared (640 MiB for the
/// Alpine desktop), the kernel is tens of MiB, the framebuffer is
/// width x height x 4. On top of those, `WORKING_RESERVE_MIB` covers what
/// grows while the machine runs: each machine's overlay of written disk
/// blocks, the video encoder, TLS and HTTP buffers, and the compressed
/// snapshot a `/snapshot` builds in memory before it uploads.
const WORKING_RESERVE_MIB: u64 = 512;

fn auto_ram_mib(cfg: &Config, images: &Images) -> Option<u64> {
    let ceiling = slice_mib()?;
    let fixed = (images.kernel.len() + images.disk.len()) as u64 >> 20;
    let fb = (cfg.fb_w * cfg.fb_h * 4) >> 20;
    let overhead = fixed + fb + WORKING_RESERVE_MIB;
    // saturating: a slice smaller than the overhead lands on the floor
    // rather than wrapping into a giant guest
    let ram = ceiling.saturating_sub(overhead).clamp(128, RAM_MIB_MAX);
    eprintln!(
        "[risc-box] ramMiB auto: slice {} MiB - (images {} + framebuffer {} + working {}) = {} MiB guest RAM",
        ceiling, fixed, fb, WORKING_RESERVE_MIB, ram
    );
    if ceiling < overhead + 128 {
        eprintln!(
            "[risc-box]   the slice barely covers this app's own footprint; buy more cpuShare or use a smaller disk image"
        );
    }
    Some(ram)
}

fn new_emulator(cfg: &Config, ram_mib: u64) -> Emulator {
    let mut emu = Emulator::new(new_terminal());
    // before setup_program: that's where the RAM is sized and the DTB memory
    // node gets synced to it
    emu.setup_ram_bytes(ram_mib * 1024 * 1024);
    // Display size and clock source, both of which the guest reads exactly once
    // at boot: the DTB node for the framebuffer, the timebase for the clock.
    if cfg.fb_w != 1024 || cfg.fb_h != 768 {
        match emu.set_framebuffer_size(cfg.fb_w as u32, cfg.fb_h as u32)
            && display::set_size(cfg.fb_w as usize, cfg.fb_h as usize)
        {
            true => {}
            false => eprintln!(
                "[risc-box] display {}x{} rejected (must be even and fit 8 MiB); staying at {}x{}",
                cfg.fb_w,
                cfg.fb_h,
                display::fb_w(),
                display::fb_h()
            ),
        }
    }
    if cfg.realtime {
        emu.set_wall_clock(true);
    }
    emu
}

/// The steps after the guest's memory is in place, shared by every path.
fn finish_machine(emu: &mut Emulator, cfg: &Config) {
    #[cfg(feature = "aot")]
    {
        emu.aot_enable();
        eprintln!("[risc-box] aot dispatcher on: {} baked regions", emu.aot_baked());
    }
    if cfg.net_enabled {
        emu.setup_network(Box::new(HostNet::new()));
    }
}

/// A machine brought up, and how: resumed from a snapshot or booted cold.
struct Booted {
    emu: Emulator,
    restored: Option<SnapshotInfo>,
}

/// Bring the MAIN machine up. With `want_snapshot` and a cached snapshot
/// that matches these images and settings, the machine RESUMES: the base
/// disk is shared in, the snapshot's RAM is adopted copy-on-write, its
/// devices and disk delta are laid over it, and the guest continues from
/// wherever it was — seconds, not a boot. A snapshot that does not match is
/// ignored with a log line and the machine boots cold, because a restore
/// against the wrong base would mount a filesystem whose blocks are half
/// from another image. The inflated image stays resident under its key so
/// instances can fork it without inflating again.
fn boot(app: &mut App, want_snapshot: bool) -> Result<Booted, String> {
    let cfg = &app.cfg;
    let images = app.cache.as_mut().expect("images cached before boot");
    let identity = images.identity(cfg);
    if want_snapshot {
        if let Some(key) = cfg.snapshot.clone() {
            let cached = app.root_images.get(&key).cloned();
            let image: Option<Arc<SnapshotImage>> = match (cached, images.snap_stored.as_ref()) {
                (Some(img), _) => Some(img),
                (None, Some(bytes)) => match Emulator::load_image(bytes) {
                    Ok(img) => {
                        let img = Arc::new(img);
                        app.root_images.insert(key.clone(), img.clone());
                        Some(img)
                    }
                    Err(e) => {
                        eprintln!("[risc-box] ignoring the snapshot: {e}; booting cold");
                        None
                    }
                },
                (None, None) => None,
            };
            if let Some(img) = image {
                let ram_mib = img.meta.ram_bytes >> 20;
                let verdict = if img.meta.identity != identity {
                    Err(format!(
                        "it was taken against different images or settings (its identity {:?}, this deployment's {:?})",
                        img.meta.identity, identity
                    ))
                } else if ram_mib != cfg.ram_mib {
                    Err(format!(
                        "it holds {} MiB of guest RAM but this deployment is configured for {} MiB",
                        ram_mib, cfg.ram_mib
                    ))
                } else if img.meta.disk_len as usize != images.disk.len() {
                    Err(format!(
                        "it expects a {}-byte base disk but the fs object expands to {} bytes",
                        img.meta.disk_len,
                        images.disk.len()
                    ))
                } else {
                    Ok(())
                };
                match verdict {
                    Ok(()) => {
                        eprintln!(
                            "[risc-box] resuming from snapshot {key} (taken at unix {}, emulator {})",
                            img.meta.taken_unix, img.meta.emu_version
                        );
                        let mut emu = new_emulator(cfg, ram_mib);
                        emu.setup_filesystem_shared(images.disk.clone());
                        let t = Instant::now();
                        let info = emu
                            .restore_image(&img, &identity)
                            .map_err(|e| format!("snapshot restore failed: {e}"))?;
                        eprintln!(
                            "[risc-box] restored in {:.2}s: {}/{} RAM pages shared, {} disk blocks replayed",
                            t.elapsed().as_secs_f64(),
                            info.ram_pages_kept,
                            info.ram_pages_total,
                            info.delta_blocks
                        );
                        finish_machine(&mut emu, cfg);
                        return Ok(Booted { emu, restored: Some(info) });
                    }
                    Err(why) => eprintln!("[risc-box] ignoring the snapshot: {why}; booting cold"),
                }
            }
        }
    }

    let mut emu = new_emulator(cfg, cfg.ram_mib);
    emu.setup_program(images.kernel.clone());
    emu.setup_filesystem_shared(images.disk.clone());
    match &images.dtb {
        Some(dtb) => emu.setup_dtb(dtb.clone()),
        None => {
            // The built-in device tree seeds the kernel's random pool so the
            // desktop does not wait ~160 s for entropy that never comes — but
            // it shipped one FIXED seed, so every cold boot of every
            // deployment started from the same pool. Fresh bytes per boot.
            use rand_core::RngCore;
            let mut seed = [0u8; 64];
            rand_core::OsRng.fill_bytes(&mut seed);
            if !emu.seed_rng(&seed) {
                eprintln!("[risc-box] dtb: rng-seed not found; the guest boots with the built-in seed");
            }
        }
    }
    finish_machine(&mut emu, cfg);
    Ok(Booted { emu, restored: None })
}

/// Resolve what an instance forks from into a resident image. "main" takes
/// the live main machine as it is right now (its RAM becomes shared; the
/// fork costs milliseconds and no bucket round trip); anything else is a
/// snapshot key — the config's root, cached from the images fetch, or any
/// other object, fetched now with the credentials the images used.
fn root_image(app: &mut App, from: &str) -> Result<(String, Arc<SnapshotImage>), String> {
    let identity = app
        .cache
        .as_ref()
        .map(|c| c.identity(&app.cfg))
        .ok_or("no images fetched yet: start the main machine first")?;
    if from == MAIN_ID {
        let m0 = app.main_mut();
        let emu = m0.emu.as_mut().ok_or("main is not running")?;
        let img = Arc::new(emu.image(&identity));
        let key = format!("main@{}", img.meta.taken_unix);
        app.root_images.insert(key.clone(), img.clone());
        return Ok((key, img));
    }
    if let Some(img) = app.root_images.get(from) {
        return Ok((from.to_string(), img.clone()));
    }
    let cfg = &app.cfg;
    let cache = app.cache.as_ref().expect("checked above");
    let bytes: Vec<u8> = match (cfg.snapshot.as_deref() == Some(from), cache.snap_stored.as_ref()) {
        (true, Some(b)) => b.clone(),
        _ => {
            let ep = Endpoint::parse(&cfg.endpoint, &cfg.region)?;
            eprintln!("[risc-box] fetching root snapshot s3://{}/{from}", cfg.bucket);
            s3::get_object(&ep, &cfg.bucket, from, app.live_creds.as_ref(), &mut progress_logger("root"))
                .map_err(|e| format!("fetch root snapshot {from}: {e}"))?
        }
    };
    let img = Emulator::load_image(&bytes).map_err(|e| format!("root snapshot {from}: {e}"))?;
    if img.meta.identity != identity {
        return Err(format!(
            "root snapshot {from} was taken against different images or settings (its identity {:?}, this deployment's {:?})",
            img.meta.identity, identity
        ));
    }
    if img.meta.disk_len as usize != cache.disk.len() {
        return Err(format!(
            "root snapshot {from} expects a {}-byte base disk, the fs object expands to {}",
            img.meta.disk_len,
            cache.disk.len()
        ));
    }
    let img = Arc::new(img);
    app.root_images.insert(from.to_string(), img.clone());
    Ok((from.to_string(), img))
}

/// A fresh machine forked from an image: shares the base disk and the
/// image's pages, gets its own console, NIC (outbound only) and overlay.
fn fork_emulator(app: &App, img: &SnapshotImage) -> Result<(Emulator, SnapshotInfo), String> {
    let cfg = &app.cfg;
    let cache = app.cache.as_ref().ok_or("no images fetched yet")?;
    let identity = cache.identity(cfg);
    let mut emu = new_emulator(cfg, img.meta.ram_bytes >> 20);
    emu.setup_filesystem_shared(cache.disk.clone());
    let info = emu.restore_image(img, &identity).map_err(|e| format!("fork failed: {e}"))?;
    finish_machine(&mut emu, cfg);
    Ok((emu, info))
}

/// Drop an image nobody forks from any more. Bucket roots stay: they are the
/// root every next instance wants. Images taken from the live main machine
/// are transient — they hold main's pages as they were at that moment, and
/// only their instances care.
fn reap_images(app: &mut App) {
    app.root_images.retain(|k, img| !k.starts_with("main@") || Arc::strong_count(img) > 1);
}

/// Instance ids are short, URL-safe and never "main".
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id != MAIN_ID
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn fresh_id(app: &App) -> String {
    use rand_core::RngCore;
    loop {
        let mut b = [0u8; 4];
        rand_core::OsRng.fill_bytes(&mut b);
        let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
        if app.find(&id).is_none() {
            return id;
        }
    }
}

// ---- request routing -------------------------------------------------------

fn route(app: &mut App, server: &mut Server, key: usize, req: Request) {
    // The static shell, its assets, and liveness stay open so the page can
    // load and prompt for a key; everything that reveals or drives a machine
    // is gated when api_key is set — including whether an instance exists.
    let open = matches!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/") | ("GET", "/ping") | ("GET", "/a/xterm.js") | ("GET", "/a/xterm.css")
    );
    if !open && !authorized(&req, &app.cfg) {
        return server.respond(key, json(401, "Unauthorized", err("api key required")));
    }
    // `/i/<id>/<route>` addresses an instance; a bare route is the main
    // machine. `/i/<id>` alone is its status (GET) or its removal (DELETE).
    let (mi, path): (usize, String) = match req.path.strip_prefix("/i/") {
        Some(rest) => {
            let (id, tail) = match rest.split_once('/') {
                Some((id, tail)) => (id, format!("/{tail}")),
                None => (rest, "/".to_string()),
            };
            match app.find(id) {
                Some(mi) => (mi, tail),
                None => return server.respond(key, json(404, "Not Found", err("no such instance"))),
            }
        }
        None => (0, req.path.clone()),
    };
    let sub = mi != 0;
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") if !sub => server.respond(
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
        ("GET", "/status") | ("GET", "/") => server.respond(key, json(200, "OK", app.status_json(mi))),
        ("GET", "/instances") if !sub => server.respond(key, json(200, "OK", app.instances_json())),
        ("POST", "/instances") if !sub => instances_create(app, server, key, &req.body),
        ("DELETE", "/") | ("POST", "/delete") if sub => instance_delete(app, server, key, mi),
        ("GET", "/console") => {
            // hand the late joiner the retained scrollback as the first frame
            let m = &app.machines[mi];
            let sb: Vec<u8> = m.scrollback.iter().copied().collect();
            let initial = if sb.is_empty() {
                String::new()
            } else {
                format!("data: {}\n\n", b64(&sb))
            };
            let topic = m.console_topic();
            server.upgrade_sse(key, &topic, &initial);
        }
        ("POST", "/start") if !sub => {
            if app.main().phase == Phase::Running {
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
            let snapshot = v.get("snapshot").and_then(|x| x.as_bool());
            app.pending = Some(Start { creds, reset, snapshot });
            app.retry = 0; // an operator start gets a fresh retry budget
            app.retry_at = None;
            app.retry_start = None;
            server.respond(key, json(202, "Accepted", "{\"ok\":true,\"phase\":\"loading\"}".into()));
        }
        ("POST", "/start") => instance_start(app, server, key, mi),
        ("POST", "/input") => {
            let m = &mut app.machines[mi];
            if m.running() {
                // run full batches until the UART has had time to drain this
                // input (it polls its terminal every ~230k ticks, one byte per
                // poll), else the idle throttle would add ~100ms per keystroke
                m.push_input(&req.body);
                server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
            } else {
                server.respond(key, json(409, "Conflict", err("machine is not running")));
            }
        }
        ("POST", "/hid") => hid_inner(&mut app.machines[mi], server, key, &req.body, true),
        // The streamed variant: this request never ends and is never answered;
        // each newline-delimited body line arrives back through poll() as a
        // synthesized /hid-stream-event and is injected with zero per-batch
        // framing or response work (see httpd::upgrade_instream). Main only:
        // the synthesized path has no instance in it.
        ("POST", "/hid-stream") if !sub => server.upgrade_instream(key, "/hid-stream-event"),
        ("POST", "/hid-stream-event") if !sub => hid_inner(&mut app.machines[0], server, key, &req.body, false),
        ("POST", "/exec") => exec(app, server, key, &req.body, mi),
        ("POST", "/save") if !sub => save(app, server, key),
        ("POST", "/save") => server.respond(key, json(403, "Forbidden", err("save is for the main machine (its disk is the saveKey's); snapshot an instance instead"))),
        ("POST", "/snapshot") => snapshot(app, server, key, &req.body, mi),
        ("POST", "/stop") => {
            let m = &mut app.machines[mi];
            m.halt();
            if mi == 0 {
                app.display.reset();
                worker::reset();
                app.venc = None;
            }
            reap_images(app);
            server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
        }
        ("GET", "/display") if !sub => {
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
        // GET /audio — take what the sound card has played since the last
        // call: {"rate":48000,"channels":2,"playing":true,"dropped":0,
        // "pcm":"<base64 s16le interleaved>"}. Taking is destructive, so one
        // consumer at a time; `max` caps the bite (default 64 KiB).
        //
        // Deliberately a pull, not an SSE stream: the device paces the guest
        // by NOT completing its playback buffers until the ring drains (see
        // emu/src/device/virtio_snd.rs), so whoever pulls sets the clock. A
        // consumer that stops pulling stalls the guest's writes rather than
        // running ahead of real time, which is the behaviour a sound card has.
        ("GET", "/audio") if !sub => {
            if !app.main().running() {
                return server.respond(key, json(409, "Conflict", err("machine is not running")));
            }
            // `?stream=1` is how a player should take audio: an SSE stream the
            // loop pushes into as the card plays, rather than a poll. Polling
            // cost a request round trip per chunk — over the fleet's relay that
            // is 100-400 ms of jitter on a stream consumed in 5 ms frames, so
            // the listener alternately starved (a chop) and sat on a backlog
            // (delay). Each event carries its own rate/channels because the
            // guest can reopen the card with different ones.
            if form_get(&req.query, "stream").is_some() {
                let emu = app.main_mut().emu.as_mut().expect("emu present (checked above)");
                let (rate, channels, _playing, _pending, _dropped) = emu.audio_state();
                let initial = format!(
                    "event: format\ndata: {{\"rate\":{},\"channels\":{}}}\n\n",
                    rate, channels
                );
                return server.upgrade_sse(key, "audio", &initial);
            }
            let max = form_get(&req.query, "max")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(64 * 1024)
                .min(512 * 1024);
            let emu = app.machines[0].emu.as_mut().expect("emu present (checked above)");
            let (rate, channels, playing, _pending, dropped) = emu.audio_state();
            let mut pcm = emu.take_audio(max);
            app.opl.mix(emu, &mut pcm, rate, channels);
            let body = format!(
                "{{\"rate\":{},\"channels\":{},\"playing\":{},\"dropped\":{},\"bytes\":{},\"pcm\":\"{}\"}}",
                rate, channels, playing, dropped, pcm.len(), b64(&pcm)
            );
            server.respond(key, json(200, "OK", body))
        }
        ("GET", "/video") if !sub => {
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
        ("POST", "/video-key") if !sub => {
            worker::force_key();
            if let Some((_, enc)) = app.venc.as_mut() {
                enc.force_keyframe();
            }
            server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
        }
        ("GET", "/fb.png") => match app.machines[mi].emu.as_ref() {
            Some(emu) if app.machines[mi].phase != Phase::Idle => {
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
        ("GET", "/frame.jpg") => match app.machines[mi].emu.as_ref() {
            Some(emu) if app.machines[mi].phase != Phase::Idle => {
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
        // Pull-paced frame delivery: the band events the scan already
        // produced, from `since` on, in one bounded response. The client's
        // in-flight window is one reply deep, so its latency is its own
        // link's, not the megabytes of relay buffering a pushed stream
        // accumulates when production outruns the link (which is what made
        // a driven cursor sit seconds behind a smooth picture). `resync`
        // tells the client its `since` fell out of the ring; a full-frame
        // scan is already scheduled and a later poll re-bases it.
        ("GET", "/fb.bands") if !sub => {
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
        // Raw framebuffer (packed RGB, FB_W x FB_H, no header) — the frame source
        // for a HARDWARE encoder. The wasm app can't call NVENC, so the native
        // GPU bridge (gs-bridge) pulls raw frames here and NVENC-encodes them
        // on the GPU (the H200 on the fleet; a dev GPU locally).
        ("GET", "/fb.rgb") => match app.machines[mi].emu.as_ref() {
            Some(emu) if app.machines[mi].phase != Phase::Idle => {
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

// ---- instances -------------------------------------------------------------

/// POST /instances — fork a new machine. Body: {"from"?: "main" | "<snapshot
/// key>", "id"?: "<id>"}. `from` defaults to the config's `snapshot` key (the
/// root). The instance is created RUNNING, resumed from the image, with the
/// restoreExec hook applied. Answers the new machine's summary.
fn instances_create(app: &mut App, server: &mut Server, key: usize, body: &[u8]) {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let from = v
        .get("from")
        .and_then(|f| f.as_str())
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .or_else(|| app.cfg.snapshot.clone());
    let Some(from) = from else {
        return server.respond(key, json(400, "Bad Request", err(
            "nothing to fork from: pass {\"from\": \"main\" | \"<snapshot key>\"} or set `snapshot` in the config",
        )));
    };
    let id = match v.get("id").and_then(|i| i.as_str()) {
        Some(id) if valid_id(id) => id.to_string(),
        Some(_) => return server.respond(key, json(400, "Bad Request", err("id must be 1-32 [A-Za-z0-9_-] characters and not \"main\""))),
        None => fresh_id(app),
    };
    if app.find(&id).is_some() {
        return server.respond(key, json(409, "Conflict", err("an instance with that id exists")));
    }
    if app.machines.len() >= app.cfg.instances_max {
        return server.respond(key, json(409, "Conflict", err(&format!(
            "instance limit reached ({} machines, instances.max = {})",
            app.machines.len(),
            app.cfg.instances_max
        ))));
    }
    // By ticket: bringing a machine up runs the restoreExec hook inline,
    // which flushes the server, which may compact the connection list under
    // this request's index (see `exec`).
    let Some(ticket) = server.hold(key) else { return };
    match instance_bring_up(app, server, &id, &from) {
        Ok(mi) => server.release(ticket, json(201, "Created", app.machines[mi].summary_json())),
        Err(e) => {
            reap_images(app);
            server.release(ticket, json(500, "Error", err(&e)))
        }
    };
}

/// Fork `from` into a machine called `id` (a new record, or an existing
/// halted one) and run the post-restore hook. The memory budget is checked
/// against what the image and the base disk already cost plus a modest
/// allowance for the pages the new guest will touch.
fn instance_bring_up(app: &mut App, server: &mut Server, id: &str, from: &str) -> Result<usize, String> {
    let (image_key, image) = root_image(app, from)?;
    let budget = app.cfg.instances_max_bytes as usize;
    let used = app.footprint();
    const ALLOWANCE: usize = 64 << 20;
    if used + ALLOWANCE > budget {
        return Err(format!(
            "memory budget exhausted: {} MiB in use of instances.maxBytes = {} MiB",
            used >> 20,
            budget >> 20
        ));
    }
    let t = Instant::now();
    let (emu, info) = fork_emulator(app, &image)?;
    let restore_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mi = match app.find(id) {
        Some(mi) => mi,
        None => {
            app.machines.push(Machine::new(id, from));
            app.machines.len() - 1
        }
    };
    let cfg_net = (app.cfg.net_enabled, app.cfg.net_outbound);
    let title = app.cfg.title.clone();
    let m = &mut app.machines[mi];
    m.origin = from.to_string();
    m.emu = Some(emu);
    m.image = Some(image);
    m.image_key = Some(image_key);
    m.phase = Phase::Running;
    m.error = None;
    m.ram_mib = info.ram_bytes >> 20;
    m.boot_at = Some(Instant::now());
    m.instret = 0;
    m.scrollback.clear();
    m.console_total = 0;
    m.restored = true;
    m.restore_ms = restore_ms;
    m.restore_taken_unix = info.taken_unix;
    if cfg_net.0 && m.net.is_none() {
        // outbound NAT like main's, no inbound forwards: those are the
        // deployment's ports and they belong to the main machine
        m.net = Some(NetStack::new(&[], cfg_net.1));
    }
    eprintln!(
        "[risc-box] instance {id} FORKED from {from} in {restore_ms:.0}ms: {} MiB, {}/{} pages shared, {} disk blocks — {title}",
        info.ram_bytes >> 20,
        info.ram_pages_kept,
        info.ram_pages_total,
        info.delta_blocks
    );
    run_restore_hook(app, server, mi);
    Ok(mi)
}

/// POST /i/<id>/start — bring a halted instance back by re-forking its
/// origin. It is a NEW fork of the same image, not a resume of what it was
/// when it stopped (a stop drops the machine's pages by design).
fn instance_start(app: &mut App, server: &mut Server, key: usize, mi: usize) {
    if app.machines[mi].running() {
        return server.respond(key, json(409, "Conflict", err("already running")));
    }
    let id = app.machines[mi].id.clone();
    let from = app.machines[mi].image_key.clone().unwrap_or_else(|| app.machines[mi].origin.clone());
    let Some(ticket) = server.hold(key) else { return };
    match instance_bring_up(app, server, &id, &from) {
        Ok(mi) => server.release(ticket, json(200, "OK", app.machines[mi].summary_json())),
        Err(e) => {
            let m = &mut app.machines[mi];
            m.error = Some(e.clone());
            m.phase = Phase::Error;
            reap_images(app);
            server.release(ticket, json(500, "Error", err(&e)))
        }
    };
}

/// DELETE /i/<id> — stop and forget an instance. Its pages, overlay and
/// console go with it; an image only it forked from goes too.
fn instance_delete(app: &mut App, server: &mut Server, key: usize, mi: usize) {
    if mi == 0 {
        return server.respond(key, json(403, "Forbidden", err("the main machine cannot be deleted; /stop it")));
    }
    let m = app.machines.remove(mi);
    drop(m);
    reap_images(app);
    server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
}

// ---- /hid ------------------------------------------------------------------

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
fn hid_inner(m: &mut Machine, server: &mut Server, key: usize, body: &[u8], respond: bool) {
    if !m.running() {
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
    let emu = m.emu.as_mut().expect("emu present (checked above)");
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
    // X repaints promptly instead of at the idle-throttle rate — but only when
    // something actually landed, and only briefly. See INPUT_BOOST_TURNS.
    if n > 0 && m.boost_hold == 0 {
        m.input_boost = m.input_boost.max(INPUT_BOOST_TURNS);
    }
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
/// stepping this machine's CPU inline (the others pause) and pumping its NIC
/// so a networked command still works, broadcasting the same bytes to
/// console watchers, and flushing periodically so SSE heartbeats keep going.
fn exec(app: &mut App, server: &mut Server, key: usize, body: &[u8], mi: usize) {
    if !app.cfg.exec_enabled {
        return server.respond(key, json(403, "Forbidden", err("exec is disabled on this deployment")));
    }
    if !app.machines[mi].running() {
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
    let cmd = cmd.to_string();
    // Park the request by TICKET, not by key: exec_run flushes the server
    // while it blocks, and a flush reaps dead connections by compacting the
    // list, so the index this request arrived under can point at another
    // connection (or nothing) by the time the answer exists. That is the
    // same reason /fb.bands long-polls hold; a dropped response here read
    // as "exec hangs" from outside.
    let Some(ticket) = server.hold(key) else { return };
    let out = exec_run(&app.cfg, &mut app.machines[mi], server, &cmd, timeout, max_out);
    let payload = match out.error {
        None => format!(
            "{{\"ok\":true,\"exitCode\":{},\"output\":\"{}\",\"truncated\":{},\"ms\":{}}}",
            out.exit_code,
            httpd::json_escape(&out.output),
            out.truncated,
            out.ms
        ),
        Some(e) => format!(
            "{{\"ok\":false,\"error\":\"{}\",\"output\":\"{}\",\"truncated\":{},\"ms\":{}}}",
            httpd::json_escape(&e),
            httpd::json_escape(&out.output),
            out.truncated,
            out.ms
        ),
    };
    server.release(ticket, json(200, "OK", payload));
}

/// What one console command came back with. `error` set means the command
/// did not complete (no prompt, or timed out); `output` is whatever came
/// back either way.
struct ExecOutcome {
    exit_code: i64,
    output: String,
    truncated: bool,
    ms: u128,
    error: Option<String>,
}

/// The console-driving core of /exec, shared with the post-restore hook.
/// Blocks the event loop for up to `timeout` (login and command together).
fn exec_run(cfg: &Config, m: &mut Machine, server: &mut Server, cmd: &str, timeout: Duration, max_out: usize) -> ExecOutcome {
    let seq = m.exec_seq;
    m.exec_seq = m.exec_seq.wrapping_add(1);
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

    let user = cfg.exec_user.clone();
    let pass = cfg.exec_password.clone().unwrap_or_default();
    let began = Instant::now();
    let mut cap: Vec<u8> = Vec::new();
    let mut last_flush = began;

    // Phase 1 — reach a shell prompt. A bare newline draws a prompt from an
    // open shell, or `login:` from a getty, which we answer from the configured
    // (passwordless-root by default) credentials. The command is only sent once
    // a prompt is in hand, so a login prompt can never consume it as a username.
    m.push_input(b"\n");
    let ready_budget = (timeout / 2).min(Duration::from_secs(10));
    let (mut sent_user, mut sent_pass, mut ready) = (false, false, false);
    while began.elapsed() < ready_budget {
        exec_pump(m, server, &mut cap, &mut last_flush);
        if tail_is_prompt(&cap) {
            ready = true;
            break;
        }
        if !sent_user && contains(&cap, b"ogin:") {
            m.push_input(format!("{user}\n").as_bytes());
            sent_user = true;
        } else if sent_user && !sent_pass && contains(&cap, b"assword:") {
            m.push_input(format!("{pass}\n").as_bytes());
            sent_pass = true;
        }
    }
    if !ready {
        let tail = &cap[cap.len().saturating_sub(800)..];
        return ExecOutcome {
            exit_code: -1,
            output: String::from_utf8_lossy(tail).into_owned(),
            truncated: false,
            ms: began.elapsed().as_millis(),
            error: Some(format!(
                "guest shell not ready: no prompt appeared on the serial console within {}s (is a getty running on ttyS0, or is the guest still booting?)",
                ready_budget.as_secs()
            )),
        };
    }

    // Phase 2 — send the command and wait for the closing marker (with its
    // whole exit-code line, i.e. a newline after it).
    let cmd_off = cap.len();
    m.push_input(line.as_bytes());
    let mut end_at: Option<usize> = None;
    while began.elapsed() < timeout {
        exec_pump(m, server, &mut cap, &mut last_flush);
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

    ExecOutcome {
        exit_code,
        output: text,
        truncated,
        ms: began.elapsed().as_millis(),
        error: match ok {
            true => None,
            false => Some(format!("exec timed out after {}s", timeout.as_secs())),
        },
    }
}

/// One event-loop turn's worth of guest work, for the inline /exec wait: step
/// the CPU, drain the UART into the console (scrollback + SSE + the capture
/// buffer), pump the NIC, and periodically flush so SSE heartbeats still fire.
fn exec_pump(m: &mut Machine, server: &mut Server, cap: &mut Vec<u8>, last_flush: &mut Instant) {
    let topic = m.console_topic();
    let mut chunk: Vec<u8> = Vec::new();
    if let Some(emu) = m.emu.as_mut() {
        emu.run_n(TICK_BATCH);
        m.instret += TICK_BATCH;
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
        if let Some(stack) = m.net.as_mut() {
            let backend = emu.get_mut_cpu().get_mut_mmu().get_mut_net().get_mut_backend();
            stack.pump(backend.as_mut());
        }
    }
    if !chunk.is_empty() {
        m.console_total += chunk.len() as u64;
        for &b in &chunk {
            if m.scrollback.len() >= SCROLLBACK {
                m.scrollback.pop_front();
            }
            m.scrollback.push_back(b);
        }
        server.broadcast(&topic, &format!("data: {}", b64(&chunk)));
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

// ---- /snapshot, /save ------------------------------------------------------

/// POST /snapshot — serialize the running machine and PUT it to a snapshot
/// key. For the main machine the key defaults to the config's; an instance
/// must name one. When the main machine's snapshot goes to the config's key,
/// it becomes the cached root — the next /start resumes from it and the next
/// instance forks it. Body: {"key"?: "<s3 key>", "level"?: 1..9}. Blocks the
/// event loop for the serialize + upload, like /save does.
///
/// Take it when the machine is QUIET: a TCP connection the guest holds open
/// at this moment (an ssh session, a download) is a real host socket that
/// cannot be in the snapshot, so on resume the guest finds it dead.
fn snapshot(app: &mut App, server: &mut Server, key: usize, body: &[u8], mi: usize) {
    if app.cfg.read_only {
        return server.respond(key, json(403, "Forbidden", err("this machine is read-only")));
    }
    if !app.machines[mi].running() {
        return server.respond(key, json(409, "Conflict", err("machine is not running")));
    }
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let save_key = v
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .or_else(|| if mi == 0 { app.cfg.snapshot_save_key.clone() } else { None });
    let Some(save_key) = save_key else {
        return server.respond(key, json(400, "Bad Request", err(
            "no snapshot key: pass {\"key\": \"<s3 key>\"} (the main machine defaults to the config's `snapshot`)",
        )));
    };
    let level = v
        .get("level")
        .and_then(|l| l.as_u64())
        .map(|l| l.clamp(1, 9) as u8)
        .unwrap_or(app.cfg.snapshot_level);
    let Some(identity) = app.cache.as_ref().map(|c| c.identity(&app.cfg)) else {
        return server.respond(key, json(409, "Conflict", err("no images cached to bind the snapshot to")));
    };
    let ep = match Endpoint::parse(&app.cfg.endpoint, &app.cfg.region) {
        Ok(e) => e,
        Err(e) => return server.respond(key, json(500, "Error", err(&e))),
    };
    let emu = app.machines[mi].emu.as_ref().expect("emu present (checked above)");
    let t0 = Instant::now();
    let (data, info) = emu.snapshot(&identity, level);
    let snap_ms = t0.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "[risc-box] snapshot of {}: {} bytes in {:.0}ms ({}/{} RAM pages kept, {} disk blocks changed); uploading to s3://{}/{save_key}",
        app.machines[mi].id, data.len(), snap_ms, info.ram_pages_kept, info.ram_pages_total, info.delta_blocks, app.cfg.bucket
    );
    let t1 = Instant::now();
    match s3::put_object(&ep, &app.cfg.bucket, &save_key, app.live_creds.as_ref(), &data) {
        Ok(()) => {
            let upload_ms = t1.elapsed().as_secs_f64() * 1000.0;
            let bytes = data.len();
            eprintln!("[risc-box] snapshot uploaded in {upload_ms:.0}ms");
            // A snapshot written to the config's root key IS the new root:
            // cache it for the next start, and retire the stale inflated
            // image so the next fork inflates this one.
            if app.cfg.snapshot.as_deref() == Some(save_key.as_str()) {
                if let Some(cache) = app.cache.as_mut() {
                    cache.snap_stored = Some(data);
                }
                app.root_images.remove(&save_key);
            } else {
                app.root_images.remove(&save_key);
            }
            app.machines[mi].last_snapshot = Some(save_key.clone());
            server.respond(
                key,
                json(200, "OK", format!(
                    "{{\"ok\":true,\"key\":\"{}\",\"bytes\":{},\"ramPagesKept\":{},\"ramPagesTotal\":{},\"deltaBlocks\":{},\"snapshotMs\":{:.0},\"uploadMs\":{:.0}}}",
                    httpd::json_escape(&save_key), bytes, info.ram_pages_kept, info.ram_pages_total,
                    info.delta_blocks, snap_ms, upload_ms
                )),
            )
        }
        Err(e) => server.respond(key, json(502, "Bad Gateway", err(&format!("snapshot upload: {e}")))),
    }
}

fn save(app: &mut App, server: &mut Server, key: usize) {
    if app.cfg.read_only {
        return server.respond(key, json(403, "Forbidden", err("this machine is read-only")));
    }
    let Some(save_key) = app.cfg.save_key.clone() else {
        return server.respond(key, json(400, "Bad Request", err("no saveKey configured")));
    };
    let Some(emu) = app.machines[0].emu.as_mut() else {
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
            app.machines[0].last_save = Some(save_key.clone());
            server.respond(
                key,
                json(200, "OK", format!("{{\"ok\":true,\"saved\":\"{}\",\"bytes\":{}}}",
                    httpd::json_escape(&save_key), disk.len())),
            )
        }
        Err(e) => server.respond(key, json(502, "Bad Gateway", err(&e))),
    }
}

// ---- start -----------------------------------------------------------------

/// Perform a queued /start of the MAIN machine: fetch (or reuse cached)
/// images and boot — or, when a matching snapshot is cached, resume.
fn do_start(app: &mut App, server: &mut Server, start: Start) {
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
                // `"ramMiB": "auto"`: now that the images are in hand (the
                // disk is the biggest thing sharing this address space with
                // the guest), size the machine to what is left of the slice.
                // Re-resolved on every fetch: a new disk image changes the
                // arithmetic, and a machine only reads this at boot.
                if app.cfg.ram_auto {
                    if let Some(ram) = auto_ram_mib(&app.cfg, app.cache.as_ref().unwrap()) {
                        app.cfg.ram_mib = ram;
                        // the whole-box budget follows the same ceiling unless
                        // the config named one: an auto-sized main machine
                        // plus forks must not outrun the slice, and the app
                        // refusing a fork beats the engine killing the process
                        if !app.cfg.instances_max_bytes_set {
                            if let Some(ceiling) = slice_mib() {
                                app.cfg.instances_max_bytes =
                                    (ceiling.saturating_sub(WORKING_RESERVE_MIB / 2)) << 20;
                            }
                        }
                    } else {
                        eprintln!(
                            "[risc-box] ramMiB auto: no ENCLAVE_MEM_MB from the host; keeping {} MiB",
                            app.cfg.ram_mib
                        );
                    }
                }
                // fresh objects: every resident image was inflated from the
                // old ones (instances already forked keep theirs alive)
                app.root_images.clear();
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
                        snapshot: start.snapshot,
                    });
                }
                let m = app.main_mut();
                m.error = Some(e);
                m.phase = Phase::Error;
                return;
            }
        }
    }
    let want_snapshot = start.snapshot.unwrap_or(true);
    let booted = match boot(app, want_snapshot) {
        Ok(b) => b,
        Err(e) if want_snapshot => {
            // A snapshot that failed mid-restore is retired so the machine
            // still comes up, cold, from the base images.
            eprintln!("[risc-box] {e}; dropping the snapshot and booting cold");
            if let Some(k) = app.cfg.snapshot.clone() {
                app.root_images.remove(&k);
            }
            if let Some(c) = app.cache.as_mut() {
                c.snap_stored = None;
            }
            match boot(app, false) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[risc-box] start failed: {e}");
                    let m = app.main_mut();
                    m.error = Some(e);
                    m.phase = Phase::Error;
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!("[risc-box] start failed: {e}");
            let m = app.main_mut();
            m.error = Some(e);
            m.phase = Phase::Error;
            return;
        }
    };
    let restored = booted.restored;
    let ram_mib = app.cfg.ram_mib;
    let title = app.cfg.title.clone();
    let net = match app.cfg.net_enabled && app.main().net.is_none() {
        // listeners live for the whole process: created on the first start
        true => Some(NetStack::new(&app.cfg.forwards, app.cfg.net_outbound)),
        false => None,
    };
    let m = app.main_mut();
    m.emu = Some(booted.emu);
    m.origin = "config".into();
    m.ram_mib = ram_mib;
    m.instret = 0;
    m.boot_at = Some(Instant::now());
    m.scrollback.clear();
    m.console_total = 0;
    m.error = None;
    m.phase = Phase::Running;
    m.restored = restored.is_some();
    m.restore_taken_unix = restored.as_ref().map_or(0, |i| i.taken_unix);
    m.restore_ms = 0.0;
    if net.is_some() {
        m.net = net;
    }
    app.display.reset(); // fresh machine, fresh screen: next watched scan ships a full frame
    worker::reset();
    app.venc = None; // fresh machine, fresh encoder (next watcher gets a keyframe)
    match &restored {
        Some(info) => eprintln!(
            "[risc-box] machine RESUMED from snapshot: {title} (taken at unix {})",
            info.taken_unix
        ),
        None => eprintln!("[risc-box] machine running: {title}"),
    }
    // The resumed guest still thinks it is the moment the snapshot was
    // taken. The hook is the operator's chance to tell it otherwise.
    if restored.is_some() {
        run_restore_hook(app, server, 0);
    }
}

/// Run the configured `restoreExec` on a machine that was just resumed or
/// forked. Blocks the loop for at most its timeout; failures are logged, never
/// fatal — the machine is up either way.
fn run_restore_hook(app: &mut App, server: &mut Server, mi: usize) {
    let Some(cmd) = app.cfg.restore_exec.clone() else { return };
    if !app.cfg.exec_enabled {
        eprintln!("[risc-box] restoreExec set but exec is disabled; skipping");
        return;
    }
    let cmd = fill_hook(&cmd);
    let timeout = Duration::from_secs(app.cfg.restore_exec_timeout_s);
    let t = Instant::now();
    let id = app.machines[mi].id.clone();
    let out = exec_run(&app.cfg, &mut app.machines[mi], server, &cmd, timeout, EXEC_DEFAULT_MAX_BYTES);
    match out.error {
        None => eprintln!(
            "[risc-box] restoreExec on {id} done in {:.1}s (exit {})",
            t.elapsed().as_secs_f64(),
            out.exit_code
        ),
        Some(e) => eprintln!("[risc-box] restoreExec on {id} failed: {e}"),
    }
}

/// Expand the placeholders a post-restore hook may use: `{epoch}` is the
/// host's UNIX time in seconds, `{entropy}` 64 fresh random bytes as hex.
fn fill_hook(cmd: &str) -> String {
    use rand_core::RngCore;
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut bytes = [0u8; 64];
    rand_core::OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    cmd.replace("{epoch}", &epoch.to_string()).replace("{entropy}", &hex)
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

    // What the platform actually handed us, by NAME only — never a value.
    // Names are already public (they are in the app config); values are not,
    // and never appear here.
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
    let mut main = Machine::new(MAIN_ID, "config");
    if unconfigured {
        main.error = Some(format!(
            "configuration incomplete: {} not set — set the deployment's config/secrets and restart",
            missing.join(", ")
        ));
    }
    let mut app = App {
        cfg,
        machines: vec![main],
        root_images: HashMap::new(),
        opl: opl::Opl::new(),
        pending: if autostart { Some(Start { creds: None, reset: false, snapshot: None }) } else { None },
        cache: None,
        live_creds: None,
        sent_frames: 0,
        sent_fps: 0.0,
        sent_at: Instant::now(),
        sent_mark: 0,
        video_frames: 0,
        video_fps: 0.0,
        video_mark: 0,
        turn_max_ms: 0.0,
        turn_max_detail: String::new(),
        display: Display::new(),
        pull: BandRing::new(),
        pull_seen: None,
        pull_waiters: Vec::new(),
        gs: None,
        gs_tried: false,
        fb_scanned: None,
        fb_overlay_frame: None,
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
        // The main machine's user-mode network, listeners included, lives for
        // the whole process so the deployment's ports answer from the start.
        app.machines[0].net = Some(NetStack::new(&app.cfg.forwards, app.cfg.net_outbound));
        match app.cfg.net_outbound {
            true => eprintln!("[risc-box] net: outbound NAT enabled (tcp/udp/dns/icmp-echo); disable with net.outbound=false"),
            false => eprintln!("[risc-box] net: outbound disabled — inbound forwards only"),
        }
    }
    eprintln!(
        "[risc-box] instances: up to {} machines within {} MiB",
        app.cfg.instances_max,
        app.cfg.instances_max_bytes >> 20
    );

    // Periodic health line. The HTTP surface already reports all of this, but
    // it is not always reachable: a PRIVATE deployment has no public data path,
    // so its log is the only window into it, and that is exactly when you most
    // want to know whether the guest is running, wedged, or quietly stopped.
    // One line a minute is cheap enough to leave on always.
    const HEARTBEAT: Duration = Duration::from_secs(60);
    let mut last_heartbeat = Instant::now();
    // Last time the loop yielded its OS thread to the host runtime. Used to
    // guarantee the runtime periodically gets a slice to ACCEPT new
    // connections even while a video watcher keeps the loop busy every turn.
    let mut last_yield = Instant::now();

    loop {
        let t0 = Instant::now();
        for (key, req) in server.poll(MAX_BODY) {
            route(&mut app, &mut server, key, req);
        }
        let t1 = Instant::now();

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
            // Guest MIPS is the number worth watching over time: it moves with
            // what else the loop is doing (scanning the framebuffer, encoding
            // video), and a fall to zero on a "running" machine is the shape
            // of a wedged guest.
            let m = app.main();
            let secs = m.boot_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
            let mips = match secs > 0.0 {
                true => m.instret as f64 / 1e6 / secs,
                false => 0.0,
            };
            let idle = m.emu.as_ref().map_or(false, |e| e.get_cpu().is_idle());
            let running = app.machines.iter().filter(|m| m.running()).count();
            eprintln!(
                "[risc-box] heartbeat: phase={} up={secs:.0}s instret={:.2}G mips={mips:.1} \
                 guest_idle={idle} console={}KiB watchers={}/{} machines={}/{} footprint={}MiB{} turn_max={:.0}ms [{}]",
                phase_name(m.phase),
                m.instret as f64 / 1e9,
                m.console_total / 1024,
                server.sse_count("display"),
                server.sse_count("video"),
                running,
                app.machines.len(),
                app.footprint() >> 20,
                m.error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default(),
                app.turn_max_ms,
                app.turn_max_detail,
            );
            app.turn_max_ms = 0.0;
            app.turn_max_detail.clear();
        }

        // Get responses (the 202 for /start, errors, etc.) onto the wire
        // BEFORE any blocking S3 work in do_start, so the browser isn't left
        // hanging on the fetch.
        server.flush();

        // A failed boot fetch left a scheduled retry: re-queue it when due.
        if app.main().phase == Phase::Error && app.pending.is_none() {
            if let Some(t) = app.retry_at {
                if Instant::now() >= t {
                    app.retry_at = None;
                    app.retry += 1;
                    app.pending = Some(app.retry_start.take().unwrap_or(Start {
                        creds: None,
                        reset: false,
                        snapshot: None,
                    }));
                }
            }
        }

        if let Some(start) = app.pending.take() {
            app.main_mut().phase = Phase::Running; // optimistic; do_start flips to Error on failure
            let t = Instant::now();
            do_start(&mut app, &mut server, start);
            if app.main().restored {
                app.main_mut().restore_ms = t.elapsed().as_secs_f64() * 1000.0;
            }
        }

        let t2 = Instant::now();
        let mut busy = false;

        // ---- step every machine ------------------------------------------
        //
        // Round-robin, one batch each per turn. The turn's instruction budget
        // is split between the machines that are actually running code, so a
        // crowded box has turns about as long as a quiet one had and every
        // guest advances on every turn; a machine parked in WFI takes an
        // idle batch, which costs the host almost nothing (timers still
        // tick). The main machine's streams are handled after this pass.
        let n_busy = app
            .machines
            .iter()
            .filter(|m| m.running() && (m.input_boost > 0 || !m.emu.as_ref().unwrap().get_cpu().is_idle()))
            .count()
            .max(1) as u64;
        let share = (TICK_BATCH / n_busy).max(MIN_BATCH);
        for m in app.machines.iter_mut() {
            if !m.running() {
                continue;
            }
            let topic = m.console_topic();
            let emu = m.emu.as_mut().expect("running implies emu");
            let parked = m.input_boost == 0 && emu.get_cpu().is_idle();
            let batch = match parked {
                true => IDLE_BATCH,
                false => share,
            };
            // A guest that is RUNNING has already paced this turn: it just
            // spent a batch of real work, and the loop must not add a sleep
            // on top. `busy` used to be set only by console bytes and
            // encoded frames, so a compute-bound guest that prints nothing —
            // a game, a build, a long boot phase — was silently throttled by
            // a millisecond every turn. At the ~6 ms a batch takes that is a
            // quarter of the machine, given away for nothing.
            busy |= !parked;
            // Cap how long the boost may run UNBROKEN. Each accepted input
            // re-arms it, so a client sending faster than it decays pins
            // the loop in full batches forever: the display worker then
            // never gets the core and video production collapses even
            // though every turn still looks healthy. Past the cap the boost
            // is dropped and cannot re-arm for BOOST_HOLD_TURNS, which
            // hands the worker a guaranteed window no input rate can take
            // away. An isolated event never reaches the cap, so a keystroke
            // keeps the full boost it was given.
            if m.input_boost > 0 {
                m.boost_run += 1;
                if m.boost_run >= BOOST_RUN_MAX {
                    m.input_boost = 0;
                    m.boost_hold = BOOST_HOLD_TURNS;
                    m.boost_run = 0;
                }
            } else {
                m.boost_run = 0;
                m.boost_hold = m.boost_hold.saturating_sub(1);
            }
            m.input_boost = m.input_boost.saturating_sub(1);
            // batched entry point: per-instruction loop overhead is
            // amortized inside the emulator, and a WFI-parked guest
            // consumes the batch without spinning (idle turns cost the
            // loop almost nothing, leaving the budget to scan/encode).
            emu.run_n(batch);
            m.instret += batch;
            // Presented frames per real second.
            {
                let bytes = emu.fb_bytes().wrapping_add(emu.gpu_flush_bytes());
                let now = Instant::now();
                let dt = now.duration_since(m.fps_at).as_secs_f64();
                if dt >= 1.0 {
                    let per = (display::fb_bytes() as f64).max(1.0);
                    m.fps_now = bytes.wrapping_sub(m.fps_bytes) as f64 / per / dt;
                    m.fps_bytes = bytes;
                    m.fps_at = now;
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
                m.console_total += chunk.len() as u64;
                for &b in &chunk {
                    if m.scrollback.len() >= SCROLLBACK {
                        m.scrollback.pop_front();
                    }
                    m.scrollback.push_back(b);
                }
                server.broadcast(&topic, &format!("data: {}", b64(&chunk)));
                busy = true;
            }
            // exchange ethernet frames between the guest NIC and the
            // user-mode network; traffic in flight lifts the WFI throttle
            // so forwarded connections stay snappy. The boost outlives the
            // frames by ~0.5s of guest CPU: interactive protocols (ping's
            // 1s cadence, TCP handshakes) sleep between packets, and
            // dropping straight back to the idle batch would stretch
            // guest time ~7x mid-conversation.
            if let Some(stack) = m.net.as_mut() {
                let backend = emu.get_mut_cpu().get_mut_mmu().get_mut_net().get_mut_backend();
                if stack.pump(backend.as_mut()) {
                    m.input_boost = m.input_boost.max(NET_BOOST_TURNS);
                    busy = true;
                }
            }
        }

        // ---- the main machine's streams: audio, display, video -------------
        {
            let sent_rate_due = {
                let now = Instant::now();
                let sdt = now.duration_since(app.sent_at).as_secs_f64();
                if sdt >= 1.0 {
                    app.sent_fps = (app.sent_frames - app.sent_mark) as f64 / sdt;
                    app.sent_mark = app.sent_frames;
                    app.video_fps = (app.video_frames - app.video_mark) as f64 / sdt;
                    app.video_mark = app.video_frames;
                    app.sent_at = now;
                }
            };
            let _ = sent_rate_due;
            let input_boost = app.machines[0].input_boost;
            let main_running = app.machines[0].running();
            if main_running {
                let emu = app.machines[0].emu.as_mut().expect("running implies emu");
                // Pace to the SLOWEST watcher as well as to the cost of a
                // scan. A frame is only worth producing if the last one has
                // mostly reached someone: past this backlog the extra frames
                // are not seen, they queue — and the queue ends at MAX_WBUF,
                // where the server closes the connection and the viewer loses
                // the stream entirely. On a loopback this never triggers; over
                // a relay, at 1024x768, it triggered within a second.
                const SSE_BACKLOG_LIMIT: usize = 192 * 1024;
                // Audio first, and in small bites. Every byte held back here
                // is delay the player hears, and the sound card's own ring is
                // deliberately shallow, so a listener that is kept fed never
                // needs a deep buffer anywhere downstream.
                if server.sse_count("audio") > 0 {
                    let (rate, channels, _playing, pending, _dropped) = emu.audio_state();
                    if pending >= AUDIO_MIN_CHUNK {
                        // ALWAYS take, even when the listener is behind. The
                        // card's ring is the guest's problem — leave it full
                        // and the device drops the audio instead, which is
                        // what the chopping was (7-11 kB/s of 38 thrown away
                        // with a listener attached the whole time).
                        let mut pcm = emu.take_audio(AUDIO_MAX_CHUNK);
                        // Music is synthesised HERE, natively, and summed
                        // into the card's PCM — see src/opl.rs. Generating
                        // exactly as many frames as the card produced borrows
                        // the card's real-time clock, so music and effects
                        // stay in step without a second timer.
                        app.opl.mix(emu, &mut pcm, rate, channels);
                        // Send unconditionally: the httpd already protects
                        // itself (starve at SSE_SKIP_WBUF, close at MAX_WBUF)
                        // and the player trims stale audio in its own ring.
                        if !pcm.is_empty() {
                            server.broadcast(
                                "audio",
                                &format!("data: {{\"r\":{},\"c\":{},\"d\":\"{}\"}}",
                                         rate, channels, b64(&pcm)),
                            );
                        }
                    }
                }

                let display_backed_up = server.sse_backlog("display") > SSE_BACKLOG_LIMIT;
                let video_backed_up = server.sse_backlog("video") > SSE_BACKLOG_LIMIT;
                let pull_watching =
                    app.pull_seen.map_or(false, |t| t.elapsed() < Duration::from_secs(3));
                let watching_display =
                    (server.sse_count("display") > 0 && !display_backed_up) || pull_watching;
                let watching_video = server.sse_count("video") > 0 && !video_backed_up;
                // Keep the display scan at its fast floor while input is recent
                // (the same boost window the CPU uses) — but only when the
                // screen was QUIET: when the frame is already animating the
                // floor-paced scan carries every change anyway, and halving
                // the floor just doubles scan+deflate work exactly while the
                // player is providing input. Boost from stillness, never from
                // motion.
                let snap = input_boost > 0 && app.fb_still > 0;
                // A video watcher needs frames at the encoder's cadence even
                // when the band diff has nothing to say: bands are not even
                // computed for it, so "still" is structurally true and the
                // backoff otherwise parks a live stream at the 100 ms ceiling.
                let scan_still = if snap || watching_video { 0 } else { app.fb_still };
                if worker::available() {
                    // A video watcher paces on the video interval, not on the
                    // scan's cost-share backoff: with a worker the EXPENSIVE
                    // half — the encode — is not on this thread at all; the
                    // guest pays only the capture.
                    let interval = match watching_video {
                        true => worker::VIDEO_MIN_INTERVAL,
                        false => display::scan_interval_boosted(
                            app.fb_cost, scan_still, snap),
                    };
                    let due = app.fb_scanned.map_or(true, |t| t.elapsed() >= interval);
                    let overlay_frame = Display::completed_frame_id(emu);
                    let unchanged = overlay_frame.is_some()
                        && overlay_frame == app.fb_overlay_frame
                        && !worker::needs_frame();
                    // Two jobs in flight, not one: the worker's encode
                    // otherwise serializes with the capture handoff and the
                    // whole pipeline runs at encode+turnaround instead of
                    // max(encode, capture). Depth 2 keeps the worker saturated
                    // and costs at most one frame of staleness (~16 ms).
                    if (watching_display || watching_video) && due && worker::inflight() < 2
                        && (watching_video || !unchanged) {
                        let began = Instant::now();
                        let mut buf = worker::take_buffer();
                        let damage = Display::capture_damage(emu, &mut buf);
                        worker::submit(worker::Job {
                            frame: buf,
                            want_bands: watching_display,
                            want_video: watching_video,
                            damage,
                        });
                        app.fb_overlay_frame = overlay_frame;
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
                    // Pace the encoder by what it COSTS, not by the clock:
                    // after each frame, wait until at least VIDEO_COST_RATIO
                    // times as long has been spent NOT encoding, so the guest
                    // keeps the large majority of the thread either way.
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
                        Display::capture(emu, &mut fresh);
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

        let t3 = Instant::now();
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
                // The same coded frame goes to Moonlight as RTP. Encoding once
                // and consuming twice is the whole reason the GameStream host
                // lives in this module rather than beside it.
                if let Some(gs) = app.gs.as_mut() {
                    gs.feed_video(&f);
                }
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

        // The GameStream host: built on the first turn after the main machine
        // is running, then polled like any other server. It must never block
        // -- this is the same turn that steps the CPU.
        if app.gs.is_none() && !app.gs_tried && app.main().running() {
            app.gs_tried = true;
            app.gs = gamestream::host::build(&app.cfg);
        }
        if let Some(gs) = app.gs.as_mut() {
            busy |= gs.poll();
        }
        // Input the client sent over the control channel, injected straight
        // into the main machine -- no HTTP hop, which is the point of the host
        // living in this module. Already translated to the app's own /hid
        // shape by gamestream::control.
        for ev in gamestream::control::take_input() {
            hid_inner(&mut app.machines[0], &mut server, 0, &ev, false);
            busy = true;
        }

        let t4 = Instant::now();
        let flushed = server.flush();
        {
            let t5 = Instant::now();
            let total = (t5 - t0).as_secs_f64() * 1000.0;
            if total > app.turn_max_ms {
                app.turn_max_ms = total;
                app.turn_max_detail = format!(
                    "poll={:.0} adm={:.0} run={:.0} collect={:.0} flush={:.0}",
                    (t1 - t0).as_secs_f64() * 1000.0,
                    (t2 - t1).as_secs_f64() * 1000.0,
                    (t3 - t2).as_secs_f64() * 1000.0,
                    (t4 - t3).as_secs_f64() * 1000.0,
                    (t5 - t4).as_secs_f64() * 1000.0,
                );
            }
            if total > 250.0 {
                eprintln!("[risc-box] SLOW TURN {total:.0}ms: {}", app.turn_max_detail);
            }
        }
        // Running with real CPU work paces the loop; only sleep when idle or
        // when the running machines produced no output and moved no bytes.
        let any_running = app.machines.iter().any(|m| m.running());
        if !any_running {
            std::thread::sleep(std::time::Duration::from_millis(20));
            last_yield = std::time::Instant::now();
        } else if !busy && !flushed {
            std::thread::sleep(std::time::Duration::from_millis(1));
            last_yield = std::time::Instant::now();
        } else if !flushed && server.pending_bytes() > 0 {
            // Queued output that would not go out this turn: yield a slice so
            // the host runtime can run its stream worker. Handing bytes to a
            // wasip2 output-stream does not put them on the socket; the
            // engine's worker does, when the runtime gets to run, which on a
            // busy guest is only when we sleep. Gated on the flush having
            // moved NOTHING, so this costs the guest nothing while a stream
            // keeps up and pays 1 ms only on the turns where the engine says
            // it has no room — the backpressure signal.
            std::thread::sleep(std::time::Duration::from_millis(1));
            last_yield = std::time::Instant::now();
        } else if last_yield.elapsed() >= std::time::Duration::from_millis(16) {
            // ACCEPT FAIRNESS. The backpressure yield above only fires when
            // OUTPUT is stuck. But a busy loop whose output IS flowing still
            // starves the host runtime of the slice it needs to ACCEPT new
            // connections: the listener's accept stays gated on the runtime's
            // cooperative budget, which replenishes only when a fiber yields.
            // Yield the OS thread at most once per ~frame so the runtime can
            // drain its accept queue; costs ~1 ms/16 ms.
            std::thread::sleep(std::time::Duration::from_millis(1));
            last_yield = std::time::Instant::now();
        }
    }
}
