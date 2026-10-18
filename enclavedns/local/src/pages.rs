//! The pages a browser sees instead of "This site can't be reached".

use crate::ens::Info;
use crate::gateway::{backend_url, full, Body, Kind};
use crate::State;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use hyper::{Response, StatusCode};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

const STYLE: &str = "
:root{--bg:#fafaf9;--fg:#1c1917;--mut:#57534e;--card:#fff;--line:#e7e5e4;--acc:#0f766e;--warn:#b45309;--code:#f5f5f4}
@media (prefers-color-scheme:dark){:root{--bg:#0c0a09;--fg:#f5f5f4;--mut:#a8a29e;--card:#1c1917;--line:#292524;--acc:#2dd4bf;--warn:#fbbf24;--code:#292524}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,-apple-system,Segoe UI,sans-serif}
main{max-width:44rem;margin:0 auto;padding:3rem 1rem}
.tag{display:inline-block;font-size:.8rem;letter-spacing:.04em;text-transform:uppercase;color:var(--mut);margin-bottom:.5rem}
h1{font-size:1.6rem;line-height:1.25;margin:0 0 .75rem}
p{margin:.6rem 0}.mut{color:var(--mut)}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:1rem 1.25rem;margin:1.25rem 0}
.card h2{font-size:1rem;margin:0 0 .5rem}
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.88rem;background:var(--code);border-radius:6px}
code{padding:.1rem .35rem;overflow-wrap:anywhere}pre{padding:.75rem 1rem;overflow-x:auto;margin:.5rem 0}
.warn{color:var(--warn)}a{color:var(--acc)}
button{font:inherit;padding:.5rem 1.1rem;border-radius:8px;border:1px solid var(--acc);background:var(--acc);color:var(--bg);cursor:pointer}
dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem;margin:0}dt{color:var(--mut)}dd{margin:0;overflow-wrap:anywhere}
footer{margin-top:2rem;font-size:.85rem;color:var(--mut)}
";

fn page(status: StatusCode, title: &str, tag: &str, body: &str) -> Response<Body> {
    let html = format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>{t}</title><style>{STYLE}</style></head><body><main><div class=tag>{tag}</div><h1>{t}</h1>{body}\
<footer>EnclaveDNS Local {v} &middot; names from EnclaveDNS, content from software on this machine, never a public gateway.</footer>\
</main></body></html>",
        t = esc(title),
        tag = esc(tag),
        v = env!("CARGO_PKG_VERSION"),
    );
    let mut r = Response::new(full(html));
    *r.status_mut() = status;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

const RETRY: &str = "<p><button onclick=\"location.reload()\">Try again</button></p>";

pub struct Missing<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub content: &'a str,
    pub backend: String,
}

fn how_to_start(kind: Kind, at: &str) -> String {
    let at = esc(at);
    match kind {
        Kind::Ipfs => format!(
            "<p>EnclaveDNS Local serves IPFS sites through a kubo node's gateway, expected at <code>{at}</code>. \
Kubo fetches the content from the IPFS network and checks it against its hash.</p>\
<p>Already installed? Start it for your user:</p><pre>systemctl --user enable --now ipfs</pre>\
<p class=mut>To install it: <code>sudo pacman -S kubo</code> (Arch) or see docs.ipfs.tech, then <code>ipfs init</code>. \
If its gateway listens elsewhere, start EnclaveDNS Local with <code>--ipfs http://HOST:PORT</code>.</p>"
        ),
        Kind::Swarm => format!(
            "<p>Swarm sites are served through a Bee node's API, expected at <code>{at}</code>. \
An ultra-light Bee node is enough to read sites.</p>\
<p class=mut>Install and start Bee: docs.ethswarm.org/docs/bee/installation/quick-start. \
If its API listens elsewhere, start EnclaveDNS Local with <code>--bee http://HOST:PORT</code>.</p>"
        ),
        Kind::Arweave => format!(
            "<p>Arweave sites are served through an AR.IO gateway node, expected at <code>{at}</code>.</p>\
<p class=mut>Run one on this machine: docs.ar.io (gateways). \
If it listens elsewhere, start EnclaveDNS Local with <code>--arweave http://HOST:PORT</code>.</p>"
        ),
        Kind::Tor => format!(
            "<p>Onion services are reached through Tor's SOCKS port, expected at <code>{}</code>.</p>\
<pre>sudo pacman -S tor\nsudo systemctl enable --now tor</pre>\
<p class=mut>If Tor's SOCKS port is elsewhere, start EnclaveDNS Local with <code>--tor HOST:PORT</code>.</p>",
            at.trim_start_matches("socks5://")
        ),
    }
}

pub fn not_detected(m: &Missing) -> Response<Body> {
    let what = match m.kind {
        Kind::Ipfs => "No IPFS node was detected",
        Kind::Swarm => "No Swarm node was detected",
        Kind::Arweave => "No Arweave gateway was detected",
        Kind::Tor => "Tor was not detected",
    };
    let body = format!(
        "<p><b>{name}</b> exists, and its site is on <b>{proto}</b>. To open it, this machine needs software that speaks {proto}, \
and none answered.</p>\
<div class=card><h2>Start {proto} on this machine</h2>{how}</div>\
<div class=card><h2>The site's address</h2><p><code>{content}</code></p>\
<p class=mut>You can open it with any {proto} software you trust. EnclaveDNS Local never sends it to a public gateway.</p></div>{RETRY}",
        name = esc(m.name),
        proto = m.kind.label(),
        how = how_to_start(m.kind, &m.backend),
        content = esc(m.content),
    );
    page(StatusCode::SERVICE_UNAVAILABLE, what, m.name, &body)
}

pub fn backend_failed(m: &Missing, err: &str) -> Response<Body> {
    let body = format!(
        "<p>{proto} software is running at <code>{at}</code>, but the request for <b>{name}</b> failed:</p><pre>{err}</pre>\
<p class=mut>Content: <code>{content}</code>. A new site can take a while to find on the network; trying again often works.</p>{RETRY}",
        proto = m.kind.label(),
        at = esc(&m.backend),
        name = esc(m.name),
        err = esc(err),
        content = esc(m.content),
    );
    page(StatusCode::BAD_GATEWAY, &format!("{} could not fetch this site", m.kind.label()), m.name, &body)
}

pub fn no_name(name: &str) -> Response<Body> {
    let body = format!(
        "<p>ENS has no record of <b>{n}</b>: nobody has registered it, or it has no resolver set.</p>\
<p class=mut>Check the spelling. ENS names end in <code>.eth</code>.</p>",
        n = esc(name)
    );
    page(StatusCode::NOT_FOUND, "This ENS name does not exist", name, &body)
}

pub fn unreachable(name: &str, enclavedns: &str, err: &str) -> Response<Body> {
    let body = format!(
        "<p>EnclaveDNS Local asks EnclaveDNS (<code>{e}</code>) what <b>{n}</b> points at, and got no answer:</p><pre>{err}</pre>\
<p class=mut>EnclaveDNS reads ENS from several Ethereum providers at once and answers only when they agree. \
It may be restarting, or the providers may be slow.</p>{RETRY}",
        e = esc(enclavedns),
        n = esc(name),
        err = esc(err)
    );
    page(StatusCode::BAD_GATEWAY, "EnclaveDNS did not answer", name, &body)
}

pub fn unsupported(info: &Info, ch: &str) -> Response<Body> {
    let body = format!(
        "<p><b>{n}</b> points at content in a format EnclaveDNS Local does not open:</p><pre>{ch}</pre>\
<p class=mut>It opens IPFS, IPNS, Swarm, Arweave and Tor onion sites. Skynet, the other format ENS defines, shut down in 2023.</p>{addr}",
        n = esc(&info.name),
        ch = esc(ch),
        addr = info.address.as_deref().map(|a| format!("<p class=mut>The name's address: <code>{}</code></p>", esc(a))).unwrap_or_default()
    );
    page(StatusCode::NOT_IMPLEMENTED, "This site uses an unsupported format", &info.name, &body)
}

pub fn address_only(info: &Info) -> Response<Body> {
    let mut rows = String::new();
    if let Some(a) = &info.address {
        rows.push_str(&format!("<dt>Address</dt><dd><code>{}</code></dd>", esc(a)));
    }
    let mut texts: Vec<_> = info.text.iter().collect();
    texts.sort();
    for (k, v) in texts {
        rows.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(k), esc(v)));
    }
    let body = format!(
        "<p><b>{n}</b> is a registered ENS name, but it does not point at a website: it has no content hash and no <code>url</code> record.</p>{card}",
        n = esc(&info.name),
        card = if rows.is_empty() { String::new() } else { format!("<div class=card><h2>What it holds</h2><dl>{rows}</dl></div>") }
    );
    page(StatusCode::OK, "This name has no website", &info.name, &body)
}

pub fn redirect(url: &str) -> Response<Body> {
    let mut r = page(StatusCode::FOUND, "Redirecting", "url record", &format!("<p><a href=\"{u}\">{u}</a></p>", u = esc(url)));
    if let Ok(v) = HeaderValue::from_str(url) {
        r.headers_mut().insert(LOCATION, v);
    }
    r
}

pub async fn home(st: &State) -> Response<Body> {
    let mut rows = String::new();
    for k in Kind::ALL {
        let up = st.backends.up(st, k).await;
        rows.push_str(&format!(
            "<dt>{}</dt><dd>{} <span class=mut>({})</span></dd>",
            k.label(),
            if up { "detected" } else { "<span class=warn>not detected</span>" },
            esc(&backend_url(st, k))
        ));
    }
    let body = format!(
        "<p>This machine opens <code>.eth</code> sites through EnclaveDNS Local: names come from EnclaveDNS \
(<code>{e}</code>), and content from the software below.</p><div class=card><h2>Local software</h2><dl>{rows}</dl></div>",
        e = esc(st.ens.base())
    );
    page(StatusCode::OK, "EnclaveDNS Local", "status", &body)
}
