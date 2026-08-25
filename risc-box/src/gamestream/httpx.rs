//! The GameStream control surface: discovery, pairing and launch.
//!
//! Ported from the native bridge with the accept loops removed. The bridge ran
//! a blocking listener per port; the guest has no threads, so `host.rs` polls
//! and calls into the pure request handlers kept here -- `route` takes a path
//! and returns a body, which is the shape that ports unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use crate::gamestream::pair::{self, Outcome, PairState};
use crate::gamestream::session::{
    Session, APP_VERSION, GFE_VERSION, PORT_HTTP, PORT_HTTPS, PORT_RTSP,
};
use std::io::{Read, Write};
// The GameStream HTTP control surface.
//
// Two listeners: plain HTTP on :47989 (discovery + pairing, which happens
// before there is any client cert to authenticate with) and TLS on :47984
// for everything post-pair. The TLS listener demands a client certificate
// and then checks it against the certs we stored at pairing time — that is
// the entire authorization model, and it is why /applist and /launch are
// HTTPS-only.
//
// Every response is HTTP 200 with an XML body; errors are carried in the
// root element's status_code attribute, matching GFE and Sunshine.




pub struct Server {
    pub pair: Arc<PairState>,
    /// The active streaming session, if one has been launched.
    pub session: std::sync::Mutex<Option<Arc<Session>>>,
    /// Called with a freshly minted session when /launch succeeds.
    pub on_launch: Box<dyn Fn(Arc<Session>) + Send + Sync>,
    pub host_name: String,
    pub unique_id: String,
}

fn xml(fields: &[(&str, String)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">");
    for (k, v) in fields {
        s.push_str(&format!("<{k}>{v}</{k}>"));
    }
    s.push_str("</root>");
    s
}

fn xml_error(code: u16, message: &str, body: &[(&str, String)]) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"{code}\" status_message=\"{message}\">"
    );
    for (k, v) in body {
        s.push_str(&format!("<{k}>{v}</{k}>"));
    }
    s.push_str("</root>");
    s
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), urldecode(v)))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len().saturating_sub(1))
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// `/serverinfo`. Over HTTPS, the presence of a `uniqueid` argument is what
/// sets PairStatus=1 — reaching the TLS listener at all already proves the
/// client is paired.
fn serverinfo(srv: &Server, args: &HashMap<String, String>, https: bool) -> String {
    let paired = https && args.contains_key("uniqueid");
    let running = srv.session.lock().unwrap().is_some();

    xml(&[
        ("hostname", srv.host_name.clone()),
        ("appversion", APP_VERSION.into()),
        ("GfeVersion", GFE_VERSION.into()),
        ("uniqueid", srv.unique_id.clone()),
        ("HttpsPort", PORT_HTTPS.to_string()),
        ("ExternalPort", PORT_HTTP.to_string()),
        ("MaxLumaPixelsHEVC", "0".into()),
        ("mac", "00:00:00:00:00:00".into()),
        ("LocalIP", "127.0.0.1".into()),
        // SCM_H264 only — the NVENC pipeline produces H.264.
        ("ServerCodecModeSupport", "1".into()),
        ("PairStatus", if paired { "1" } else { "0" }.into()),
        ("currentgame", if running { "1" } else { "0" }.into()),
        (
            "state",
            if running { "SUNSHINE_SERVER_BUSY" } else { "SUNSHINE_SERVER_FREE" }.into(),
        ),
    ])
}

fn applist() -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">");
    s.push_str("<App><IsHdrSupported>0</IsHdrSupported><AppTitle>RISC Box Desktop</AppTitle><ID>1</ID></App>");
    s.push_str("</root>");
    s
}

/// `/launch` — mint a session from the client's keys and hand back the RTSP URL.
fn launch(srv: &Server, args: &HashMap<String, String>, local_ip: &str, resume: bool) -> String {
    let tag = if resume { "resume" } else { "gamesession" };

    let (Some(rikey), Some(rikeyid)) = (args.get("rikey"), args.get("rikeyid")) else {
        return xml_error(400, "Missing a required launch parameter", &[(tag, "0".into())]);
    };

    let key = from_hex(rikey);
    if key.len() != 16 {
        return xml_error(400, "Invalid rikey", &[(tag, "0".into())]);
    }
    // rikeyid is a signed decimal; its big-endian u32 form seeds the audio IV.
    let key_id: u32 = rikeyid.parse::<i64>().unwrap_or(0) as u32;

    let app_id: i32 = args.get("appid").and_then(|v| v.parse().ok()).unwrap_or(0);

    // Reuse the existing session on /resume; otherwise replace it.
    if resume {
        if srv.session.lock().unwrap().is_none() {
            return xml_error(503, "No running app to resume", &[(tag, "0".into())]);
        }
    }
    if let Some(old) = srv.session.lock().unwrap().take() {
        old.stop();
    }

    let ping_payload = pair::random_hex(8);
    let connect_data = u32::from_le_bytes(pair::random_bytes(4).try_into().unwrap());
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);

    let session = Arc::new(Session::new(id, key, key_id, ping_payload, connect_data, app_id));
    *srv.session.lock().unwrap() = Some(session.clone());
    (srv.on_launch)(session);

    eprintln!("[https] {} appid={app_id} -> rtsp://{local_ip}:{PORT_RTSP}", if resume { "resume" } else { "launch" });

    xml(&[
        ("sessionUrl0", format!("rtsp://{local_ip}:{PORT_RTSP}")),
        (tag, "1".into()),
    ])
}

fn cancel(srv: &Server) -> String {
    if let Some(s) = srv.session.lock().unwrap().take() {
        s.stop();
    }
    xml(&[("cancel", "1".into())])
}

/// Route one request. `authed` is whether a paired client cert was presented.
pub fn route(srv: &Server, path: &str, https: bool, local_ip: &str) -> Outcome {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let args = parse_query(query);
    eprintln!("[{}] > {route}", if https { "https" } else { "http" });

    match route {
        "/serverinfo" => Outcome::Reply(serverinfo(srv, &args, https)),
        "/pair" => srv.pair.handle(&args),
        "/pin" => {
            // Headless-test convenience: deliver the PIN that a real
            // deployment would show to the operator.
            let id = args.get("uniqueid").cloned().unwrap_or_default();
            let pin = args.get("pin").cloned().unwrap_or_default();
            srv.pair.submit_pin(&id, &pin);
            Outcome::Reply("<?xml version=\"1.0\"?><root status_code=\"200\"><pin>ok</pin></root>".into())
        }
        "/unpair" => {
            // Only the caller's own pairing. This endpoint is unauthenticated
            // plain HTTP, and Moonlight calls it by itself whenever it cannot
            // verify a host — so wiping every pairing here means one confused
            // client logs everyone out.
            match args.get("uniqueid") {
                Some(id) => srv.pair.unpair(id),
                None => eprintln!("[pair] /unpair without a uniqueid; ignoring"),
            }
            Outcome::Reply(xml(&[("unpaired", "1".into())]))
        }
        "/applist" if https => Outcome::Reply(applist()),
        "/launch" if https => Outcome::Reply(launch(srv, &args, local_ip, false)),
        "/resume" if https => Outcome::Reply(launch(srv, &args, local_ip, true)),
        "/cancel" if https => Outcome::Reply(cancel(srv)),
        _ => Outcome::Reply(xml_error(404, "Not Found", &[])),
    }
}

fn write_response(stream: &mut impl Write, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn read_request_path(stream: &mut impl Read) -> Option<String> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next()?;
    Some(line.split_whitespace().nth(1)?.to_string())
}
