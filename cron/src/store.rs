use crate::{config::Storage, crypt::Cipher, engine::State, net::Pending, sigv4};
use sha2::{Digest, Sha256};
pub const MAX_STATE: usize = 4 * 1024 * 1024;
/// Storage requests get this long (sia-s3 takes 20-60 s per small PUT: every
/// object uploads a full slab to its hosts).
pub const STORAGE_TIMEOUT: u64 = 120;
/// A save is at most three storage requests (PUT, its repeat, the read-back),
/// so the lease outlasts the slowest save that can still succeed.
pub const LEASE: u64 = 600;
/// An idle scheduler renews its lease this often, leaving LEASE - RENEW_AFTER
/// (> 3 x STORAGE_TIMEOUT) for the renewal itself to land.
pub const RENEW_AFTER: u64 = 240;
pub struct Store {
    cfg: Storage,
    etag: Option<String>,
    owner: String,
}
impl Store {
    pub fn new(cfg: Storage) -> Result<Self, String> {
        Ok(Self {
            cfg,
            etag: None,
            owner: crate::engine::random_id()?,
        })
    }
    fn request(
        &self,
        method: &str,
        body: &[u8],
        condition: bool,
    ) -> Result<crate::net::Reply, String> {
        let u = url::Url::parse(&self.cfg.endpoint).map_err(|_| "Invalid storage URL")?;
        let authority = u[url::Position::BeforeHost..url::Position::AfterPort].to_string();
        let path = format!(
            "/{}/{}",
            sigv4::uri_encode(&self.cfg.bucket, false),
            sigv4::uri_encode(&self.cfg.key, true)
        );
        let mut extra = vec![("content-type".into(), "application/octet-stream".into())];
        if condition {
            extra.push(match &self.etag {
                Some(t) => ("if-match".into(), t.clone()),
                None => ("if-none-match".into(), "*".into()),
            });
        }
        let headers = if self.cfg.access_key.is_empty() {
            extra
        } else {
            sigv4::sign(
                method,
                &sigv4::Endpoint {
                    authority,
                    region: self.cfg.region.clone(),
                },
                &path,
                "",
                &format!("{:x}", Sha256::digest(body)),
                &extra,
                &sigv4::Creds {
                    access_key_id: self.cfg.access_key.clone(),
                    secret_access_key: self.cfg.secret_key.clone(),
                    session_token: None,
                },
            )
        };
        Pending::start(
            method,
            &format!("{}{path}", self.cfg.endpoint.trim_end_matches('/')),
            &headers,
            body,
            STORAGE_TIMEOUT,
            MAX_STATE + 64,
        )?
        .wait()
    }
    pub fn load(&mut self, now: u64) -> Result<State, String> {
        let r = self.request("GET", &[], false)?;
        let mut s = match r.status {
            404 => State::default(),
            200 => {
                let tag = r
                    .header("etag")
                    .filter(|s| !s.is_empty())
                    .ok_or("Storage returned no ETag; conditional writes required")?
                    .to_string();
                let plain = Cipher::for_scope(&self.cfg.master_key, "state")
                    .open(&self.cfg.key, &r.body)?;
                self.etag = Some(tag);
                let s: State = serde_json::from_slice(&plain)
                    .map_err(|_| "Invalid persisted scheduler state")?;
                if s.version != 1 || s.jobs.len() > 128 || s.runs.len() > 300 {
                    return Err("Unsupported or oversized scheduler state".into());
                }
                s
            }
            _ => return Err(format!("Storage read failed (HTTP {})", r.status)),
        };
        if s.lease_until > now {
            return Err("Another scheduler owns the state lease; wait for it to expire".into());
        }
        s.recover(now);
        self.save(&mut s, now)?;
        Ok(s)
    }
    /// After an ambiguous write whose repeat met a conflict: did the first one
    /// land? Only an object that opens to exactly what we wrote is ours.
    fn adopt_if_ours(&mut self, plain: &[u8], why: &str) -> Result<(), String> {
        let fenced = |what: &str| Err(format!("State commit ambiguous ({why}); {what}; execution fenced"));
        let r = match self.request("GET", &[], false) {
            Ok(r) => r,
            Err(e) => return fenced(&format!("read-back failed: {e}")),
        };
        if r.status != 200 {
            return fenced(&format!("read-back HTTP {}", r.status));
        }
        let Some(tag) = r.header("etag").filter(|s| !s.is_empty()).map(str::to_string) else {
            return fenced("read-back had no ETag");
        };
        match Cipher::for_scope(&self.cfg.master_key, "state").open(&self.cfg.key, &r.body) {
            Ok(stored) if stored == plain => {
                self.etag = Some(tag);
                Ok(())
            }
            _ => fenced("the stored state is not this write"),
        }
    }
    /// Publish before acknowledging mutations or starting effects. An ambiguous
    /// write failure fences this process; recovery does not replay running work.
    pub fn save(&mut self, s: &mut State, now: u64) -> Result<(), String> {
        s.version = 1;
        s.revision = s
            .revision
            .checked_add(1)
            .ok_or("State revision exhausted")?;
        s.lease_owner = self.owner.clone();
        s.lease_until = now + LEASE;
        let plain = serde_json::to_vec(s).map_err(|_| "Cannot encode state")?;
        if plain.len() > MAX_STATE {
            return Err("State quota exceeded".into());
        }
        let body = Cipher::for_scope(&self.cfg.master_key, "state").seal(&self.cfg.key, &plain)?;
        // A write that fails in transit or answers 5xx is ambiguous: it may have
        // landed. The same conditional PUT is safe to repeat once: it cannot
        // overwrite anything newer. A conflict on that repeat is resolved by
        // reading the object back: exactly our plaintext (this revision, owner
        // and lease) means the first write landed; anything else fences.
        let r = match self.request("PUT", &body, true) {
            Ok(r) if r.status < 500 => r,
            first => {
                let why = match first {
                    Ok(r) => format!("HTTP {}", r.status),
                    Err(e) => e,
                };
                match self.request("PUT", &body, true) {
                    Ok(r) if r.status == 412 || r.status == 409 => return self.adopt_if_ours(&plain, &why),
                    Ok(r) => r,
                    Err(e) => {
                        return Err(format!(
                            "State commit failed ({why}; repeat: {e}); execution fenced"
                        ))
                    }
                }
            }
        };
        if r.status == 412 || r.status == 409 {
            return Err("Lost storage lease (conditional-write conflict); execution fenced".into());
        }
        if r.status != 200 {
            return Err(format!(
                "State commit failed (HTTP {}); execution fenced",
                r.status
            ));
        }
        self.etag = Some(
            r.header("etag")
                .filter(|s| !s.is_empty())
                .ok_or("Storage omitted committed ETag; execution fenced")?
                .to_string(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    /// What the fake bucket does with each PUT, in order; then it behaves.
    #[derive(Clone, Copy)]
    enum Put {
        StoreThenDrop,
        Drop,
        Normal,
    }
    struct Bucket {
        obj: Option<(Vec<u8>, String)>,
        n: u32,
        plan: Vec<Put>,
    }
    /// A conditional-write bucket on loopback: If-Match / If-None-Match like R2.
    fn bucket(plan: Vec<Put>) -> (String, Arc<Mutex<Bucket>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let b = Arc::new(Mutex::new(Bucket { obj: None, n: 0, plan }));
        let bb = b.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let mut c = c.unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let method = line.split(' ').next().unwrap_or("").to_string();
                let (mut len, mut if_match, mut if_none) = (0usize, None, false);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if let Some(v) = h.strip_prefix("if-match:").or_else(|| h.strip_prefix("If-Match:")) {
                        if_match = Some(v.trim().to_string());
                    }
                    if lower.starts_with("if-none-match:") {
                        if_none = true;
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let mut st = bb.lock().unwrap();
                let reply = |c: &mut std::net::TcpStream, status: &str, etag: Option<&str>, body: &[u8]| {
                    let tag = etag.map(|t| format!("etag: {t}\r\n")).unwrap_or_default();
                    let _ = c.write_all(format!("HTTP/1.1 {status}\r\n{tag}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                    let _ = c.write_all(body);
                };
                if method == "GET" {
                    match st.obj.clone() {
                        Some((b, t)) => reply(&mut c, "200 OK", Some(&t), &b),
                        None => reply(&mut c, "404 Not Found", None, b""),
                    }
                    continue;
                }
                let step = if st.plan.is_empty() { Put::Normal } else { st.plan.remove(0) };
                let current = st.obj.as_ref().map(|o| o.1.clone());
                let ok = match (&if_match, if_none) {
                    (Some(t), _) => current.as_deref() == Some(t.as_str()),
                    (None, true) => current.is_none(),
                    _ => true,
                };
                if matches!(step, Put::Drop) {
                    continue; // the request never landed and nobody answers
                }
                if !ok {
                    reply(&mut c, "412 Precondition Failed", None, b"");
                    continue;
                }
                st.n += 1;
                let tag = format!("\"v{}\"", st.n);
                st.obj = Some((body, tag.clone()));
                if matches!(step, Put::StoreThenDrop) {
                    continue; // it landed, but the answer is lost
                }
                reply(&mut c, "200 OK", Some(&tag), b"");
            }
        });
        (url, b)
    }
    fn store(url: &str) -> Store {
        Store::new(Storage {
            endpoint: url.into(),
            bucket: "b".into(),
            key: "cron/state".into(),
            region: "auto".into(),
            access_key: String::new(),
            secret_key: String::new(),
            master_key: "m".into(),
        })
        .unwrap()
    }

    #[test]
    fn a_write_that_landed_but_lost_its_answer_is_adopted() {
        let (url, _b) = bucket(vec![Put::Normal, Put::StoreThenDrop]);
        let mut st = store(&url);
        let mut s = st.load(1000).unwrap(); // first save: creates the object
        st.save(&mut s, 1001).expect("read-back shows the write landed");
        let rev = s.revision;
        st.save(&mut s, 1002).expect("and the store keeps going from its ETag");
        assert_eq!(s.revision, rev + 1);
    }

    #[test]
    fn a_write_that_never_landed_is_repeated() {
        let (url, _b) = bucket(vec![Put::Normal, Put::Drop]);
        let mut st = store(&url);
        let mut s = st.load(1000).unwrap();
        st.save(&mut s, 1001).expect("the repeat lands");
        st.save(&mut s, 1002).unwrap();
    }

    #[test]
    fn someone_elses_state_still_fences() {
        let (url, b) = bucket(vec![Put::Normal, Put::Drop]);
        let mut st = store(&url);
        let mut s = st.load(1000).unwrap();
        // another writer replaces the object between our load and our save
        {
            let mut g = b.lock().unwrap();
            g.n += 1;
            let other = Cipher::for_scope("m", "state").seal("cron/state", b"{\"other\":true}").unwrap();
            g.obj = Some((other, format!("\"v{}\"", g.n)));
        }
        let e = st.save(&mut s, 1001).unwrap_err();
        assert!(e.contains("fenced"), "{e}");
    }
}
