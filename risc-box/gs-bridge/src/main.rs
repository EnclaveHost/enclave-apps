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
}

fn parse_args() -> Args {
    let mut app_url = "127.0.0.1:8000".to_string();
    let mut codec = "h264_nvenc".to_string();
    let mut state_dir = dirs_state();
    // The RISC Box guest's simple-framebuffer is 800x600.
    let mut fb = (800u32, 600u32);

    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--app" if i + 1 < argv.len() => {
                app_url = argv[i + 1].clone();
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
                        eprintln!("--fb expects WxH, e.g. 800x600");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!(
                    "gs-bridge — GameStream host for RISC Box\n\n\
                       --app <host:port>   RISC Box app HTTP endpoint (default 127.0.0.1:8000)\n\
                       --codec <name>      NVENC encoder (default h264_nvenc)\n\
                       --fb <WxH>          size of the app's /fb.rgb framebuffer (default 800x600)\n\
                       --state <dir>       where to keep the server identity and paired certs"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    Args { app_url, codec, state_dir, fb }
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
fn start_session_workers(session: Arc<Session>, app: Arc<app::App>, codec: String, fb: (u32, u32)) {
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
        std::thread::spawn(move || video::run(s, a, sock, codec, fb));
    }

    // The control channel owns the ENet host and runs until teardown.
    std::thread::spawn(move || {
        control::run(session, app, || {});
    });
}

fn main() {
    let args = parse_args();
    let app = Arc::new(app::App::new(&args.app_url));

    eprintln!("[main] RISC Box app at {}", app.addr());
    eprintln!("[main] state in {}", args.state_dir.display());

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
        std::thread::spawn(move || {
            rtsp::run(
                move || launched.lock().unwrap().clone(),
                move |session| start_session_workers(session, app.clone(), codec.clone(), fb),
            );
        });
    }

    {
        let srv = srv.clone();
        std::thread::spawn(move || httpx::run_https(srv));
    }

    httpx::run_http(srv);
}
