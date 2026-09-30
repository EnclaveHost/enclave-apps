use crate::{config::Storage, crypt::Cipher, engine::State, net::Pending, sigv4};
use sha2::{Digest, Sha256};
pub const MAX_STATE: usize = 4 * 1024 * 1024;
pub const LEASE: u64 = 120;
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
            20,
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
        let r = self.request("PUT", &body, true)?;
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
