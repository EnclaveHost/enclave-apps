// gs-bridge — a GameStream host for the RISC Box desktop.
//
// A real Moonlight client discovers this host, pairs with it, negotiates a
// stream, and receives hardware-encoded video of the emulated machine while
// its input is injected back into the emulator's virtio-input HID.
//
// The pieces, and the ports they own:
//
//   :47989  HTTP    discovery + pairing            (http/pair)
//   :47984  HTTPS   applist/launch/resume/cancel   (httpx)
//   :48010  TCP     RTSP stream negotiation        (rtsp)
//   :47998  UDP     RTP video + FEC                (video)
//   :47999  UDP     ENet control, encrypted        (control)
//   :48000  UDP     RTP audio                      (audio)
//
// The video path is: the app's GET /fb.rgb -> NVENC on the GPU -> Annex-B
// access units -> RTP shards with Reed-Solomon parity. In production that
// encode runs on the fleet node's H200; the NVENC API is identical on a dev
// card, so a pipeline verified locally is the same code path.

mod app;
mod audio;
mod control;
mod crypto;
mod enet;
mod fec;
mod httpx;
mod pair;
mod screen;
mod ping;
mod rtsp;
mod session;
mod video;

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

use session::{Session, PORT_AUDIO, PORT_VIDEO};

struct Args {
    app_url: String,
    codec: String,
    state_dir: std::path::PathBuf,
    /// Dimensions of the app's /fb.rgb framebuffer.
    fb: (u32, u32),
    /// Bearer token, for a deployment whose config sets `api_key`.
    api_key: Option<String>,
    /// Fetch one frame, report what came back, and exit.
    probe: bool,
    frames: FrameSource,
}

/// Where the encoder's pictures come from.
#[derive(Clone, Copy, PartialEq)]
enum FrameSource {
    /// Bands for a remote (https) app, raw fetches for a local one.
    Auto,
    /// Mirror the /display band stream.
    Bands,
    /// Poll GET /fb.bands (pull-paced deltas; lowest latency over a link
    /// slower than the screen changes).
    Pull,
    /// GET /fb.rgb per frame.
    Raw,
}

fn parse_args() -> Args {
    let mut app_url = "127.0.0.1:8000".to_string();
    let mut codec = "h264_nvenc".to_string();
    let mut state_dir = dirs_state();
    // The RISC Box guest's simple-framebuffer, as declared in the emulator's
    // DTB and mirrored by the app's display::FB_W/FB_H.
    let mut fb = (1024u32, 768u32);
    // Taken from the environment as well as the flag so a token need not sit
    // in the process list on a shared box.
    let mut api_key = std::env::var("RISCBOX_API_KEY").ok().filter(|k| !k.is_empty());
    let mut probe = false;
    let mut frames = FrameSource::Auto;

    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--app" if i + 1 < argv.len() => {
                app_url = argv[i + 1].clone();
                i += 2;
            }
            "--api-key" if i + 1 < argv.len() => {
                api_key = Some(argv[i + 1].clone()).filter(|k| !k.is_empty());
                i += 2;
            }
            "--codec" if i + 1 < argv.len() => {
                codec = argv[i + 1].clone();
                i += 2;
            }
            "--state" if i + 1 < argv.len() => {
                state_dir = std::path::PathBuf::from(&argv[i + 1]);
                i += 2;
            }
            "--fb" if i + 1 < argv.len() => {
                let spec = argv[i + 1].clone();
                match spec.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))) {
                    Some(v) => fb = v,
                    None => {
                        eprintln!("--fb expects WxH, e.g. 1024x768");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            "--frames" if i + 1 < argv.len() => {
                frames = match argv[i + 1].as_str() {
                    "bands" => FrameSource::Bands,
                    "pull" => FrameSource::Pull,
                    "raw" => FrameSource::Raw,
                    "auto" => FrameSource::Auto,
                    other => {
                        eprintln!("--frames expects auto, bands, pull or raw (got {other})");
                        std::process::exit(2);
                    }
                };
                i += 2;
            }
            "--probe" => {
                probe = true;
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!(
                    "gs-bridge — GameStream host for RISC Box\n\n\
                       --app <url>         RISC Box app endpoint: host:port, http://… or\n\
                                           https://… (default 127.0.0.1:8000). https is how\n\
                                           you reach a deployment on the fleet, which\n\
                                           terminates TLS inside the enclave.\n\
                       --api-key <token>   bearer token if the app config sets api_key\n\
                                           (or set RISCBOX_API_KEY)\n\
                       --codec <name>      NVENC encoder (default h264_nvenc)\n\
                       --fb <WxH>          size of the app's /fb.rgb framebuffer (default 1024x768)\n\
                       --state <dir>       where to keep the server identity and paired certs\n\
                       --frames <mode>     auto (default), bands or raw. bands mirrors the\n\
                                           app's /display stream so only changed rows cross\n\
                                           the network; raw fetches a whole framebuffer per\n\
                                           frame. auto picks bands for an https app.\n\
                       --probe             fetch one frame, report it, and exit"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    Args { app_url, codec, state_dir, fb, api_key, probe, frames }
}

fn dirs_state() -> std::path::PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/state")
        })
        .join("gs-bridge")
}

/// Bring up the per-session workers once RTSP ANNOUNCE has settled the config.
fn start_session_workers(
    session: Arc<Session>,
    app: Arc<app::App>,
    screen: Option<Arc<screen::Screen>>,
    codec: String,
    fb: (u32, u32),
) {
    // Video and audio sockets are bound per session so a restart rebinds cleanly.
    let video_sock = match UdpSocket::bind(("0.0.0.0", PORT_VIDEO)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[main] failed to bind video :{PORT_VIDEO}: {e}");
            session.stop();
            return;
        }
    };
    let audio_sock = match UdpSocket::bind(("0.0.0.0", PORT_AUDIO)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[main] failed to bind audio :{PORT_AUDIO}: {e}");
            session.stop();
            return;
        }
    };

    // Learn the client's media ports from its pings.
    {
        let (s, sock) = (session.clone(), video_sock.clone());
        std::thread::spawn(move || ping::watch(s, sock, ping::video_slot, "video"));
    }
    {
        let (s, sock) = (session.clone(), audio_sock.clone());
        std::thread::spawn(move || ping::watch(s, sock, ping::audio_slot, "audio"));
    }

    {
        let (s, sock) = (session.clone(), audio_sock.clone());
        std::thread::spawn(move || audio::run(s, sock));
    }
    {
        let (s, a, sock) = (session.clone(), app.clone(), video_sock.clone());
        let sc = screen.clone();
        std::thread::spawn(move || video::run(s, a, sc, sock, codec, fb));
    }

    // The control channel owns the ENet host and runs until teardown.
    std::thread::spawn(move || {
        control::run(session, app, || {});
    });
}

fn main() {
    let args = parse_args();
    let app = Arc::new(app::App::new(&args.app_url).with_api_key(args.api_key.clone()));

    eprintln!("[main] RISC Box app at {}", app.addr());

    // --probe answers the first question anyone has when a stream does not
    // start: can this bridge reach that app at all, and is the framebuffer the
    // size we are about to hand the encoder? Both are cheap to get wrong
    // (a deployment serves https only and an api_key may be set), and both
    // produce identical symptoms much later, as a stream that connects and
    // shows nothing.
    if args.probe && args.frames == FrameSource::Bands {
        // Prove the MIRROR, not just the connection: start it, wait for bands
        // to land, and report the reconstructed picture. Inflating and
        // re-ordering BGRX into rgb24 is the part that can silently produce a
        // plausible-looking wrong image, so this also writes the frame out for
        // eyeballing against the app's own /fb.png.
        let sc = screen::Screen::start(app.clone(), args.fb.0 as usize, args.fb.1 as usize);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while sc.generation() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if sc.generation() == 0 {
            eprintln!("[probe] FAILED: no bands arrived in 30s");
            std::process::exit(1);
        }
        // The first band is not a picture. The app ships the whole frame when a
        // watcher joins, but a busy screen also has its own small rectangles in
        // flight, and whichever lands first sets generation to 1 — so snapshotting
        // there catches a game's status bar on a black field and calls it the
        // desktop. Let the bands stop arriving first: quiet for half a second, or
        // five seconds of a screen that never holds still, whichever comes first.
        let settle = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut last = sc.generation();
        let mut quiet = std::time::Instant::now();
        while std::time::Instant::now() < settle {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let now = sc.generation();
            if now != last {
                last = now;
                quiet = std::time::Instant::now();
            } else if quiet.elapsed() >= std::time::Duration::from_millis(500) {
                break;
            }
        }
        let mut buf = Vec::new();
        let gen = sc.snapshot_into(&mut buf);
        eprintln!("[probe] mirrored {} bytes after {gen} bands", buf.len());
        let distinct = {
            let mut seen = std::collections::HashSet::new();
            for px in buf.chunks_exact(3).step_by(997).take(4096) {
                seen.insert([px[0], px[1], px[2]]);
            }
            seen.len()
        };
        match distinct {
            1 => eprintln!("[probe] mirror is a SINGLE FLAT COLOUR — nothing drawn yet"),
            n => eprintln!("[probe] mirror has {n} distinct colours in a sample"),
        }
        if let Ok(path) = std::env::var("GS_PROBE_PPM") {
            let mut out = format!("P6\n{} {}\n255\n", args.fb.0, args.fb.1).into_bytes();
            out.extend_from_slice(&buf);
            match std::fs::write(&path, out) {
                Ok(()) => eprintln!("[probe] wrote {path}"),
                Err(e) => eprintln!("[probe] could not write {path}: {e}"),
            }
        }
        std::process::exit(0);
    }

    if args.probe {
        let began = std::time::Instant::now();
        match app.get("/fb.rgb") {
            Ok(f) if f.is_empty() => {
                eprintln!("[probe] FAILED: empty body from /fb.rgb — is the machine running?");
                std::process::exit(1);
            }
            Ok(f) => {
                // rgb24: three bytes per pixel, which is what the app packs
                // and what the encoder is fed ("-pix_fmt rgb24" in video.rs).
                const BYTES_PER_PIXEL: usize = 3;
                let expect = args.fb.0 as usize * args.fb.1 as usize * BYTES_PER_PIXEL;
                eprintln!(
                    "[probe] got {} bytes in {} ms",
                    f.len(),
                    began.elapsed().as_millis()
                );
                match f.len() == expect {
                    true => eprintln!("[probe] OK: matches --fb {}x{} ({expect} bytes)", args.fb.0, args.fb.1),
                    false => {
                        let px = f.len() / BYTES_PER_PIXEL;
                        eprintln!(
                            "[probe] MISMATCH: expected {expect} for {}x{}. {} pixels came back \
                             — pass the app's real framebuffer with --fb",
                            args.fb.0, args.fb.1, px
                        );
                        std::process::exit(1);
                    }
                }
                // A blank screen streams exactly as well as a desktop does, so
                // say whether there is anything on it (see the README).
                let distinct = {
                    let mut seen = std::collections::HashSet::new();
                    for px in f.chunks_exact(BYTES_PER_PIXEL).step_by(997).take(4096) {
                        seen.insert([px[0], px[1], px[2]]);
                    }
                    seen.len()
                };
                match distinct {
                    1 => eprintln!("[probe] screen is a SINGLE FLAT COLOUR — nothing is drawn yet"),
                    n => eprintln!("[probe] screen has {n} distinct colours in a sample — something is drawn"),
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[probe] FAILED to reach {}: {e}", app.addr());
                std::process::exit(1);
            }
        }
    }

    eprintln!("[main] state in {}", args.state_dir.display());

    // Where frames come from. Fetching a whole framebuffer per frame is fine
    // beside the app and hopeless across a network (2.25 MiB each, measured at
    // 2.9s against the fleet), so a remote app is mirrored from the /display
    // band stream instead: only changed rows cross the wire. --frames forces
    // either one.
    let screen = match args.frames {
        FrameSource::Pull => {
            eprintln!("[main] frames: pull-pacing /fb.bands");
            Some(screen::Screen::start_pull(app.clone(), args.fb.0 as usize, args.fb.1 as usize))
        }
        FrameSource::Bands => {
            eprintln!("[main] frames: mirroring the /display band stream");
            Some(screen::Screen::start(app.clone(), args.fb.0 as usize, args.fb.1 as usize))
        }
        FrameSource::Auto if args.app_url.starts_with("https://") => {
            eprintln!("[main] frames: mirroring the /display band stream");
            Some(screen::Screen::start(app.clone(), args.fb.0 as usize, args.fb.1 as usize))
        }
        _ => {
            eprintln!("[main] frames: fetching /fb.rgb per frame");
            None
        }
    };

    let pair_state = Arc::new(pair::PairState::load(&args.state_dir));

    // Wiring: /launch mints a session, RTSP ANNOUNCE starts its workers.
    let launched: Arc<Mutex<Option<Arc<Session>>>> = Arc::new(Mutex::new(None));

    let srv = Arc::new(httpx::Server {
        pair: pair_state,
        session: Mutex::new(None),
        on_launch: {
            let launched = launched.clone();
            Box::new(move |s: Arc<Session>| {
                *launched.lock().unwrap() = Some(s);
            })
        },
        host_name: "RISC Box".to_string(),
        unique_id: "0123456789ABCDEF".to_string(),
    });

    // RTSP resolves the launched session per connection, then starts the
    // workers once ANNOUNCE has settled the stream config.
    {
        let launched = launched.clone();
        let app = app.clone();
        let codec = args.codec.clone();
        let fb = args.fb;
        let screen = screen.clone();
        std::thread::spawn(move || {
            rtsp::run(
                move || launched.lock().unwrap().clone(),
                move |session| {
                    start_session_workers(session, app.clone(), screen.clone(), codec.clone(), fb)
                },
            );
        });
    }

    {
        let srv = srv.clone();
        std::thread::spawn(move || httpx::run_https(srv));
    }

    httpx::run_http(srv);
}
