// RTSP on :48010 — the stream negotiation handshake.
//
// For appversion 7.1.431 the client uses plain TCP (moonlight-common-c
// RtspConnection.c:950 disables its ENet path at >= 7.1.404), and it opens a
// NEW connection per request, reading the response until EOF. Two rules
// follow, and breaking either one hangs the client:
//
//   1. every response must be followed by a half-close, since there is no
//      Content-Length framing on the response side;
//   2. every response must carry at least one header (CSeq) — the client's
//      parser can only terminate a message inside its option loop, so a
//      header-less response is reported as malformed.
//
// The handshake is OPTIONS, DESCRIBE, SETUP x3, ANNOUNCE, PLAY. ANNOUNCE is
// where the stream config arrives and where the workers start; PLAY is a
// no-op that just acknowledges.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::gamestream::session::{Session, StreamConfig, PORT_AUDIO, PORT_CONTROL, PORT_VIDEO, SS_ENC_AUDIO, SS_ENC_CONTROL_V2};

/// The opaque session id we hand out; the client keeps only the part before
/// the first ';'.
const SESSION_ID: &str = "DEADBEEFCAFE;timeout = 90";

pub struct Request {
    pub command: String,
    pub target: String,
    pub cseq: i64,
    pub headers: Vec<(String, String)>,
    pub payload: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_request(raw: &str) -> Option<Request> {
    let (head, payload) = match raw.find("\r\n\r\n") {
        Some(i) => (&raw[..i], raw[i + 4..].to_string()),
        None => (raw, String::new()),
    };

    let mut lines = head.split("\r\n");
    let start = lines.next()?;
    let mut parts = start.split_whitespace();
    let command = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut cseq = -1i64;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_string();
        let v = v.trim().to_string();
        if k.eq_ignore_ascii_case("CSeq") {
            cseq = v.parse().unwrap_or(-1);
        }
        headers.push((k, v));
    }

    Some(Request { command, target, cseq, headers, payload })
}

/// Serialize a response. `CSeq` is always present — see the note above.
fn response(status: u16, message: &str, cseq: i64, extra: &[(&str, &str)], body: &str) -> String {
    let mut out = format!("RTSP/1.0 {status} {message}\r\nCSeq: {cseq}\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    out
}

/// The DESCRIBE body: what codecs and features we support.
///
/// Codec selection is by marker string — the client greps this body for
/// "AV1/90000" and "sprop-parameter-sets=AAAAAU". We advertise neither, so it
/// negotiates H.264, which is what the NVENC pipeline produces.
fn describe_body() -> String {
    let mut ss = String::new();
    // Feature flags: no pen/touch capability to report.
    ss.push_str("a=x-ss-general.featureFlags:0\n");
    // Control-stream encryption v2 is the modern path and costs little, so it
    // is both supported and requested.
    //
    // Video encryption is NOT advertised, and that is deliberate. When it was
    // offered, stock moonlight-qt 6.1.0 opted in (encryptionEnabled arrives as
    // 0x3, control+video) and then dropped ~90% of frames — the stream became a
    // 2-3 fps slideshow while every client-side stat (decode 0.75ms, network
    // latency 1ms, queue 0.25ms) stayed healthy, i.e. the frames never
    // reconstructed. A current-tree moonlight-common-c client decrypts the same
    // stream at 30fps with zero loss, so the bridge's encryption is correct
    // against the latest spec; moonlight-qt 6.1.0 ships an older
    // moonlight-common-c whose SS_ENC_VIDEO handling does not agree with it.
    // The plaintext video path is the one verified end to end against real
    // clients, so we stay on it. On a loopback or trusted-LAN bridge the picture
    // never leaves the machine anyway. Re-enable only after the encrypted path
    // is validated against the actual moonlight-qt build in use, not just the
    // headless library.
    let supported = SS_ENC_CONTROL_V2;
    ss.push_str(&format!("a=x-ss-general.encryptionSupported:{supported}\n"));
    ss.push_str(&format!("a=x-ss-general.encryptionRequested:{}\n", SS_ENC_CONTROL_V2));
    // Stereo Opus, the only layout we generate.
    ss.push_str("a=fmtp:97 surround-params=21101\n");
    ss
}

/// Pull an integer SDP attribute out of the client's ANNOUNCE body.
fn sdp_attr<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    for line in body.lines() {
        let line = line.trim_end_matches(['\r', ' ']);
        let Some(rest) = line.strip_prefix("a=") else { continue };
        let Some((k, v)) = rest.split_once(':') else { continue };
        if k == name {
            return Some(v.trim());
        }
    }
    None
}

fn sdp_u32(body: &str, name: &str) -> Option<u32> {
    sdp_attr(body, name).and_then(|v| v.parse().ok())
}

/// Apply the client's ANNOUNCE to the session config.
fn apply_announce(session: &Session, body: &str) -> Result<StreamConfig, &'static str> {
    let mut cfg = StreamConfig::default();

    cfg.width = sdp_u32(body, "x-nv-video[0].clientViewportWd").ok_or("missing clientViewportWd")?;
    cfg.height = sdp_u32(body, "x-nv-video[0].clientViewportHt").ok_or("missing clientViewportHt")?;
    cfg.fps = sdp_u32(body, "x-nv-video[0].maxFPS").ok_or("missing maxFPS")?;
    cfg.packet_size = sdp_u32(body, "x-nv-video[0].packetSize").ok_or("missing packetSize")? as usize;
    cfg.bitrate_kbps = sdp_u32(body, "x-nv-vqos[0].bw.maximumBitrateKbps").ok_or("missing maximumBitrateKbps")?;

    cfg.min_required_fec_packets = sdp_u32(body, "x-nv-vqos[0].fec.minRequiredFecPackets").unwrap_or(0) as usize;
    cfg.control_protocol_type = sdp_u32(body, "x-nv-general.useReliableUdp").unwrap_or(1);
    cfg.ml_feature_flags = sdp_u32(body, "x-ml-general.featureFlags").unwrap_or(0);
    cfg.encryption_flags = sdp_u32(body, "x-ss-general.encryptionEnabled").unwrap_or(0);
    cfg.video_format = sdp_u32(body, "x-nv-vqos[0].bitStreamFormat").unwrap_or(0);

    // NVFF_AUDIO_ENCRYPTION (0x20) in the client's feature flags means it will
    // try to decrypt audio whether or not we advertised support for it.
    let nv_flags = sdp_u32(body, "x-nv-general.featureFlags").unwrap_or(135);
    cfg.audio_encrypted = (nv_flags & 0x20) != 0 || (cfg.encryption_flags & SS_ENC_AUDIO) != 0;

    if cfg.video_format != 0 {
        // We only produce H.264; the client should not have picked anything
        // else given what DESCRIBE advertised.
        return Err("unsupported bitStreamFormat");
    }
    if cfg.packet_size < 64 || cfg.packet_size > 2048 {
        return Err("implausible packetSize");
    }

    *session.config.lock().unwrap() = cfg.clone();
    Ok(cfg)
}

fn handle(session: &Arc<Session>, req: &Request, on_announce: &dyn Fn(Arc<Session>)) -> String {
    match req.command.as_str() {
        "OPTIONS" => response(200, "OK", req.cseq, &[], ""),

        "DESCRIBE" => response(200, "OK", req.cseq, &[], &describe_body()),

        "SETUP" => {
            // target looks like "streamid=video/0/0"; take what's between
            // the first '=' and the following '/'.
            let stream = req
                .target
                .split_once('=')
                .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
                .unwrap_or("");

            let port = match stream {
                "audio" => PORT_AUDIO,
                "video" => PORT_VIDEO,
                "control" => PORT_CONTROL,
                _ => return response(404, "NOT FOUND", req.cseq, &[], ""),
            };

            let transport = format!("server_port={port}");
            let connect_data = session.connect_data.to_string();
            let mut extra: Vec<(&str, &str)> = vec![("Session", SESSION_ID), ("Transport", &transport)];
            if stream == "control" {
                extra.push(("X-SS-Connect-Data", &connect_data));
            } else {
                extra.push(("X-SS-Ping-Payload", &session.ping_payload));
            }
            response(200, "OK", req.cseq, &extra, "")
        }

        "ANNOUNCE" => match apply_announce(session, &req.payload) {
            Ok(cfg) => {
                eprintln!(
                    "[rtsp] ANNOUNCE ok: {}x{}@{} packetSize={} bitrate={}kbps ctrl={} enc={:#x} audioEnc={}",
                    cfg.width,
                    cfg.height,
                    cfg.fps,
                    cfg.packet_size,
                    cfg.bitrate_kbps,
                    cfg.control_protocol_type,
                    cfg.encryption_flags,
                    cfg.audio_encrypted
                );
                on_announce(session.clone());
                response(200, "OK", req.cseq, &[], "")
            }
            Err(e) => {
                eprintln!("[rtsp] ANNOUNCE rejected: {e}");
                response(400, "BAD REQUEST", req.cseq, &[], "")
            }
        },

        // PLAY is an acknowledgement only; the workers were already started
        // by ANNOUNCE, exactly as Sunshine does it.
        "PLAY" => response(200, "OK", req.cseq, &[], ""),

        _ => response(404, "NOT FOUND", req.cseq, &[], ""),
    }
}

fn serve_conn(session: Arc<Session>, mut stream: TcpStream, on_announce: &dyn Fn(Arc<Session>)) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));

    // The client sends its request in small segments; read until we have a
    // complete header block plus any declared payload.
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return,
        };
        raw.extend_from_slice(&buf[..n]);

        let text = String::from_utf8_lossy(&raw).to_string();
        let Some(hdr_end) = text.find("\r\n\r\n") else { continue };

        // If a Content-length was declared, wait for the whole body.
        let want: usize = text[..hdr_end]
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        if raw.len() >= hdr_end + 4 + want {
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw).to_string();
    let Some(req) = parse_request(&text) else {
        let _ = stream.write_all(response(400, "BAD REQUEST", 0, &[], "").as_bytes());
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };

    eprintln!("[rtsp] > {} {} (CSeq {})", req.command, req.target, req.cseq);
    let resp = handle(&session, &req, on_announce);

    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    // The client reads until EOF — without this it stalls for 15s and fails.
    let _ = stream.shutdown(Shutdown::Both);
}

/// Run the RTSP listener.
///
/// The session is resolved per connection via `current_session`, because
/// /launch mints it moments before the client opens its first RTSP socket.
/// `on_announce` fires once the stream config is negotiated, which is when
/// the video/audio/control workers start.
pub fn run(
    current_session: impl Fn() -> Option<Arc<Session>> + Send + Sync + 'static,
    on_announce: impl Fn(Arc<Session>) + Send + Sync + 'static,
) {
    let listener = match TcpListener::bind(("0.0.0.0", crate::gamestream::session::PORT_RTSP)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[rtsp] failed to bind :{}: {e}", crate::gamestream::session::PORT_RTSP);
            return;
        }
    };
    eprintln!("[rtsp] listening on :{}", crate::gamestream::session::PORT_RTSP);

    let on_announce = Arc::new(on_announce);
    let current_session = Arc::new(current_session);

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Some(session) = current_session() else {
            // No pending launch: Sunshine closes without responding here.
            eprintln!("[rtsp] connection with no launched session, dropping");
            let _ = conn.shutdown(Shutdown::Both);
            continue;
        };
        let on_announce = on_announce.clone();
        std::thread::spawn(move || {
            serve_conn(session, conn, &*on_announce);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_response_carries_cseq() {
        // A header-less response is unparseable by the client, so even the
        // error paths must include CSeq.
        for r in [
            response(200, "OK", 4, &[], ""),
            response(404, "NOT FOUND", 9, &[], ""),
            response(400, "BAD REQUEST", 2, &[], ""),
        ] {
            assert!(r.contains("\r\nCSeq: "), "missing CSeq in: {r:?}");
            assert!(r.starts_with("RTSP/1.0 "));
            assert!(r.contains("\r\n\r\n"), "headers must be terminated");
        }
    }

    #[test]
    fn parses_a_setup_request() {
        let raw = "SETUP streamid=video/0/0 RTSP/1.0\r\nCSeq: 4\r\n\
                   X-GS-ClientVersion: 14\r\nSession: DEADBEEFCAFE\r\n\r\n";
        let req = parse_request(raw).expect("parses");
        assert_eq!(req.command, "SETUP");
        assert_eq!(req.target, "streamid=video/0/0");
        assert_eq!(req.cseq, 4);
        assert_eq!(req.header("X-GS-ClientVersion"), Some("14"));
    }

    #[test]
    fn describe_advertises_h264_only() {
        let body = describe_body();
        assert!(!body.contains("AV1/90000"), "AV1 marker would flip codec negotiation");
        assert!(
            !body.contains("sprop-parameter-sets=AAAAAU"),
            "HEVC marker would flip codec negotiation"
        );
        assert!(body.contains("surround-params=21101"), "stereo Opus layout must be present");
    }

    #[test]
    fn announce_parses_the_clients_sdp() {
        let s = Arc::new(Session::new(1, vec![0u8; 16], 0, "p".into(), 0, 0));
        // Attribute lines carry a trailing space before CRLF, as the client sends them.
        let body = concat!(
            "v=0\r\no=android 0 14 IN IPv4 0.0.0.0\r\ns=NVIDIA Streaming Client\r\n",
            "a=x-nv-video[0].clientViewportWd:1280 \r\n",
            "a=x-nv-video[0].clientViewportHt:720 \r\n",
            "a=x-nv-video[0].maxFPS:60 \r\n",
            "a=x-nv-video[0].packetSize:1024 \r\n",
            "a=x-nv-vqos[0].bw.maximumBitrateKbps:8000 \r\n",
            "a=x-nv-vqos[0].fec.minRequiredFecPackets:2 \r\n",
            "a=x-nv-general.useReliableUdp:13 \r\n",
            "a=x-nv-general.featureFlags:135 \r\n",
            "a=x-ml-general.featureFlags:3 \r\n",
            "a=x-ss-general.encryptionEnabled:1 \r\n",
            "a=x-nv-vqos[0].bitStreamFormat:0 \r\n",
            "t=0 0\r\nm=video 47998  \r\n"
        );
        let cfg = apply_announce(&s, body).expect("accepts a well-formed ANNOUNCE");
        assert_eq!((cfg.width, cfg.height, cfg.fps), (1280, 720, 60));
        assert_eq!(cfg.packet_size, 1024);
        assert_eq!(cfg.bitrate_kbps, 8000);
        assert_eq!(cfg.min_required_fec_packets, 2);
        assert_eq!(cfg.control_protocol_type, 13);
        assert_eq!(cfg.ml_feature_flags, 3);
        assert!(!cfg.audio_encrypted, "featureFlags 135 has no audio-encryption bit");
    }

    #[test]
    fn announce_detects_requested_audio_encryption() {
        let s = Arc::new(Session::new(1, vec![0u8; 16], 0, "p".into(), 0, 0));
        let body = concat!(
            "a=x-nv-video[0].clientViewportWd:800 \r\n",
            "a=x-nv-video[0].clientViewportHt:600 \r\n",
            "a=x-nv-video[0].maxFPS:30 \r\n",
            "a=x-nv-video[0].packetSize:1024 \r\n",
            "a=x-nv-vqos[0].bw.maximumBitrateKbps:4000 \r\n",
            // 167 = 0xA7 = base | RI encryption | audio encryption
            "a=x-nv-general.featureFlags:167 \r\n"
        );
        let cfg = apply_announce(&s, body).expect("accepts");
        assert!(cfg.audio_encrypted, "bit 0x20 means the client will decrypt audio");
    }

    #[test]
    fn announce_rejects_a_missing_mandatory_attribute() {
        let s = Arc::new(Session::new(1, vec![0u8; 16], 0, "p".into(), 0, 0));
        let body = "a=x-nv-video[0].clientViewportWd:1280 \r\n";
        assert!(apply_announce(&s, body).is_err());
    }
}
