//! Public, pinned images from the host's cache (enclave docs/design/shield-blob-cache.md).
//!
//! On a Windows Shield host every byte this app fetches through its egress is a byte of a paid privacy circuit. The
//! base images (kernel, disk, snapshot) are public and pinned by sha256 in the config (`blobs`), so they are asked
//! of `blobs.enclave.internal:80` first: shield-egress dials that name to the host's cache, which fetched the object
//! once from its public `src` and serves it from disk. The bytes are checked here against the pinned sha256 before
//! anything uses them, so the host can delay or refuse an image but never substitute one. Any failure - no such
//! service (an older host, a Linux host), a refusal, a mismatch - falls back to the normal S3 path, unchanged.
use std::io::{Read, Write};

use sha2::{Digest, Sha256};

pub const HOST: &str = "blobs.enclave.internal";

/// An image pinned in the config: `"blobs": {"<s3 key>": {"sha256": "<64 hex>", "src": "https://..."}}`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub sha256: String,
    pub src: String,
}

impl Pin {
    /// A pin the cache will serve: 64 lowercase hex and an https source without a query string (the host refuses a
    /// presigned URL - it is a credential); anything else is ignored rather than tried.
    pub fn parse(v: &serde_json::Value) -> Option<Pin> {
        let sha256 = v.get("sha256")?.as_str()?.to_ascii_lowercase();
        let src = v.get("src")?.as_str()?.to_string();
        let hex = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
        (hex && src.starts_with("https://") && !src.contains('?') && !src.contains('@')).then_some(Pin { sha256, src })
    }
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(not(test))]
fn dial() -> Result<std::net::TcpStream, String> {
    crate::egress::dial(HOST, 80, Some(std::time::Duration::from_secs(10)))
}
// tests point the client at a local server instead of the reserved name
#[cfg(test)]
thread_local! { static TEST_TARGET: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) }; }
#[cfg(test)]
fn dial() -> Result<std::net::TcpStream, String> {
    let t = TEST_TARGET.with(|t| t.borrow().clone());
    std::net::TcpStream::connect(t).map_err(|e| format!("connect: {e}"))
}

struct Answer {
    status: u16,
    content_length: Option<usize>,
    range_start: Option<usize>,
    body: Vec<u8>,
    complete: bool,
}

/// One request: GET the pinned object from `from`, reading what arrives (a cut is reported, not an error).
fn get_once(pin: &Pin, from: usize, progress: &mut dyn FnMut(usize, usize)) -> Result<Answer, String> {
    let mut sock = dial()?;
    let mut head = format!("GET /sha256/{}?src={} HTTP/1.1\r\nhost: {HOST}\r\nconnection: close\r\n", pin.sha256, pct(&pin.src));
    if from > 0 {
        head.push_str(&format!("range: bytes={from}-\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).map_err(|e| format!("send: {e}"))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 64 * 1024];
    let mut head_end: Option<usize> = None;
    let (mut status, mut content_length, mut range_start): (u16, Option<usize>, Option<usize>) = (0, None, None);
    loop {
        let n = match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) if head_end.is_some() => break, // a cut mid-body: report what arrived
            Err(e) => return Err(format!("read: {e}")),
        };
        buf.extend_from_slice(&tmp[..n]);
        if head_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head_end = Some(pos + 4);
                let text = String::from_utf8_lossy(&buf[..pos]).to_string();
                let mut lines = text.split("\r\n");
                status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).ok_or("bad status line")?;
                for line in lines {
                    let Some((k, v)) = line.split_once(':') else { continue };
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => content_length = v.trim().parse().ok(),
                        "content-range" => {
                            range_start = v.trim().strip_prefix("bytes ").and_then(|r| r.split('-').next()).and_then(|n| n.trim().parse().ok())
                        }
                        _ => {}
                    }
                }
                if let Some(cl) = content_length {
                    buf.reserve_exact((pos + 4 + cl).saturating_sub(buf.len()));
                }
            }
        }
        if let (Some(he), Some(cl)) = (head_end, content_length) {
            if status == 200 || status == 206 {
                progress(from + buf.len() - he, from + cl);
            }
        }
    }
    let he = head_end.ok_or("the blob service closed before answering")?;
    buf.drain(..he);
    let complete = content_length.map_or(true, |cl| buf.len() >= cl);
    Ok(Answer { status, content_length, range_start, body: buf, complete })
}

/// The pinned object, verified, or why not (the caller then takes its normal path).
pub fn fetch(pin: &Pin, progress: &mut dyn FnMut(usize, usize)) -> Result<Vec<u8>, String> {
    let mut have: Vec<u8> = Vec::new();
    for attempt in 0..6 {
        let from = have.len();
        // the first attempt failing to connect means there is no cache here: fall back at once, no retries
        let a = match get_once(pin, from, progress) {
            Ok(a) => a,
            Err(e) if from == 0 => return Err(format!("{HOST}: {e}")),
            Err(e) => {
                eprintln!("[risc-box] blob {}: resume at {from} failed ({e}); attempt {}", &pin.sha256[..12], attempt + 1);
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        };
        match a.status {
            200 => have = a.body,                                               // whole (or the host had not cached it yet)
            206 if a.range_start == Some(from) => have.extend_from_slice(&a.body),
            s => return Err(format!("{HOST} answered {s}")),
        }
        if a.complete {
            let got = hex(&Sha256::digest(&have));
            if got != pin.sha256 {
                return Err(format!("{HOST} sent sha256 {}…, not the pinned {}…", &got[..12], &pin.sha256[..12]));
            }
            eprintln!("[risc-box] blob {}: {} bytes from the host cache, sha256 checked", &pin.sha256[..12], have.len());
            return Ok(have);
        }
        let _ = a.content_length;
        eprintln!("[risc-box] blob {}: cut at {} bytes; resuming", &pin.sha256[..12], have.len());
    }
    Err(format!("{HOST}: still incomplete after 6 attempts ({} bytes)", have.len()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn serve(script: impl Fn(usize, &str) -> Vec<u8> + Send + 'static, conns: usize) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for n in 0..conns {
                let Ok((mut s, _)) = l.accept() else { return };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                s.write_all(&script(n, &head)).ok();
            }
        });
        addr
    }
    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }
    fn pin_of(b: &[u8]) -> Pin {
        Pin { sha256: hex(&Sha256::digest(b)), src: "https://ipfs.enclave.host/ipfs/bafyexample".into() }
    }
    fn target(addr: &str) {
        TEST_TARGET.with(|t| *t.borrow_mut() = addr.to_string());
    }
    fn ok200(b: &[u8], send: usize) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", b.len()).into_bytes();
        out.extend_from_slice(&b[..send]);
        out
    }

    #[test]
    fn a_whole_answer_is_checked_and_used() {
        let b = body(150_000);
        let bb = b.clone();
        target(&serve(move |_, head| {
            assert!(head.starts_with("GET /sha256/"), "{head}");
            assert!(head.contains("src=https%3A%2F%2Fipfs.enclave.host%2Fipfs%2Fbafyexample"), "{head}");
            ok200(&bb, bb.len())
        }, 1));
        assert_eq!(fetch(&pin_of(&b), &mut |_, _| {}).unwrap(), b);
    }

    #[test]
    fn a_cut_body_resumes_with_range() {
        let b = body(120_000);
        let bb = b.clone();
        target(&serve(move |n, head| match n {
            0 => ok200(&bb, 70_000),
            _ => {
                assert!(head.contains("range: bytes=70000-"), "{head}");
                let mut out = format!("HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes 70000-{}/{}\r\n\r\n",
                    bb.len() - 70_000, bb.len() - 1, bb.len()).into_bytes();
                out.extend_from_slice(&bb[70_000..]);
                out
            }
        }, 2));
        assert_eq!(fetch(&pin_of(&b), &mut |_, _| {}).unwrap(), b);
    }

    #[test]
    fn a_wrong_object_is_refused_and_no_service_fails_at_once() {
        let b = body(10_000);
        let other = body(10_001);
        target(&serve(move |_, _| ok200(&other, other.len()), 1));
        let e = fetch(&pin_of(&b), &mut |_, _| {}).unwrap_err();
        assert!(e.contains("not the pinned"), "{e}");
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = l.local_addr().unwrap().to_string();
        drop(l);
        target(&dead);
        let t0 = std::time::Instant::now();
        assert!(fetch(&pin_of(&b), &mut |_, _| {}).is_err());
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "no retries when there is no cache");
    }
    #[test]
    fn pins_the_cache_will_not_serve_are_ignored() {
        let ok = serde_json::json!({"sha256": "AB".repeat(32), "src": "https://ipfs.enclave.host/ipfs/bafy"});
        assert_eq!(Pin::parse(&ok).unwrap().sha256, "ab".repeat(32));
        for bad in [
            serde_json::json!({"sha256": "ab", "src": "https://x/y"}),
            serde_json::json!({"sha256": "ab".repeat(32), "src": "http://x/y"}),
            serde_json::json!({"sha256": "ab".repeat(32), "src": "https://x/y?X-Amz-Signature=1"}),
            serde_json::json!({"sha256": "ab".repeat(32), "src": "https://u:p@x/y"}),
            serde_json::json!({"src": "https://x/y"}),
        ] {
            assert!(Pin::parse(&bad).is_none(), "{bad}");
        }
        assert_eq!(pct("https://a.b/c d?e"), "https%3A%2F%2Fa.b%2Fc%20d%3Fe");
    }
}
