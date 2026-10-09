//! Push webhooks: after a push commits, every configured hook whose repo
//! patterns match gets a POST of the ref updates, signed
//! `X-Depot-Signature: sha256=<hex HMAC-SHA256(secret, body)>` (GitHub's
//! scheme, so existing receivers verify it unchanged). Deliveries queue and
//! go out between requests, one at a time with a short timeout, retried
//! with backoff; a hook that keeps failing never slows a push.

use crate::app::{now, App};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub url: String,
    /// `$SECRET` for the HMAC key
    pub secret: String,
    #[serde(default = "all")]
    pub repos: Vec<String>,
}

fn all() -> Vec<String> {
    vec!["*".into()]
}

pub struct Delivery {
    hook: usize,
    body: Vec<u8>,
    tries: u32,
    not_before: Instant,
    id: String,
}

#[derive(Default)]
pub struct Queue {
    q: VecDeque<Delivery>,
    pub delivered: u64,
    pub failed: u64,
    pub last_error: Option<String>,
}

pub struct Update<'a> {
    pub name: &'a str,
    pub old: String,
    pub new: String,
}

/// Queue a delivery to every hook watching `repo`.
pub fn push_event(app: &mut App, repo: &str, pusher: &str, updates: &[Update]) {
    if app.cfg.hooks.is_empty() || updates.is_empty() {
        return;
    }
    let id = crate::seal::random_id();
    let body = json!({
        "event": "push",
        "delivery": id,
        "repository": repo,
        "pusher": pusher,
        "time": now(),
        "updates": updates.iter().map(|u| json!({"ref": u.name, "before": u.old, "after": u.new})).collect::<Vec<Value>>(),
    })
    .to_string()
    .into_bytes();
    for (i, h) in app.cfg.hooks.iter().enumerate() {
        if h.repos.iter().any(|p| crate::git::glob(p, repo)) && app.hooks.q.len() < 1000 {
            app.hooks.q.push_back(Delivery {
                hook: i,
                body: body.clone(),
                tries: 0,
                not_before: Instant::now(),
                id: id.clone(),
            });
        }
    }
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac key");
    m.update(body);
    format!("sha256={}", crate::git::hex(&m.finalize().into_bytes()))
}

fn deliver(h: &Hook, d: &Delivery) -> Result<u16, String> {
    let rest = h.url.split_once("://").map(|x| x.1).unwrap_or("");
    let (origin_rest, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let scheme = &h.url[..h.url.find("://").unwrap_or(0)];
    let mut c = crate::client::Client::new(&format!("{scheme}://{origin_rest}"))?;
    c.timeout = Duration::from_secs(5);
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "user-agent".to_string(),
            crate::git::upload::AGENT.to_string(),
        ),
        ("x-depot-event".to_string(), "push".to_string()),
        ("x-depot-delivery".to_string(), d.id.clone()),
        ("x-depot-signature".to_string(), sign(&h.secret, &d.body)),
        ("x-hub-signature-256".to_string(), sign(&h.secret, &d.body)),
    ];
    let r = c.send("POST", path, &headers, &d.body)?;
    Ok(r.status)
}

/// One delivery attempt, if one is due. True when it did anything.
pub fn tick(app: &mut App) -> bool {
    let Some(pos) = app
        .hooks
        .q
        .iter()
        .position(|d| d.not_before <= Instant::now())
    else {
        return false;
    };
    let mut d = app.hooks.q.remove(pos).unwrap();
    let Some(h) = app.cfg.hooks.get(d.hook).cloned() else {
        return true;
    };
    match deliver(&h, &d) {
        Ok(s) if (200..300).contains(&s) => app.hooks.delivered += 1,
        r => {
            let why = match r {
                Ok(s) => format!("HTTP {s}"),
                Err(e) => e,
            };
            d.tries += 1;
            if d.tries >= 6 {
                app.hooks.failed += 1;
                app.hooks.last_error = Some(format!("{}: {why} (gave up after 6 tries)", h.url));
                eprintln!("[depot] webhook {} gave up: {why}", h.url);
            } else {
                d.not_before = Instant::now() + Duration::from_secs(5 * 4u64.pow(d.tries - 1));
                app.hooks.last_error = Some(format!("{}: {why} (retrying)", h.url));
                app.hooks.q.push_back(d);
            }
        }
    }
    true
}

pub fn status(app: &App) -> Value {
    json!({ "hooks": app.cfg.hooks.len(), "queued": app.hooks.q.len(), "delivered": app.hooks.delivered,
            "failed": app.hooks.failed, "last_error": app.hooks.last_error })
}

#[cfg(test)]
mod tests {
    #[test]
    fn github_compatible_signature() {
        // GitHub's documented example: secret "It's a Secret to Everybody", payload "Hello, World!"
        assert_eq!(
            super::sign("It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }
}
