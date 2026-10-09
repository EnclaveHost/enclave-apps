//! Request routing: git smart HTTP under `/<repo>[.git]/…`, the JSON API
//! under `/api/`, and the web view at `/`.
//!
//! Credentials: HTTP Basic (the password, or the user name when the
//! password is empty, is the token), `Authorization: Bearer`, `X-Api-Key`
//! or `X-Depot-Token`, or an Enclave sign-in token in `X-Sso-Token`. The
//! platform's app gateway removes `Authorization` before a request reaches
//! the app, so behind it git is configured with
//! `http.<url>.extraHeader = X-Api-Key: <token>`.

use crate::app::App;
use crate::config::{can_read, can_write, Who};
use crate::fetch::FetchSource;
use crate::git::ingest::Limits;
use crate::git::repo::Index;
use crate::git::upload::{self, View};
use crate::git::{pkt, receive};
use crate::push::PushSink;
use crate::serve::{Handler, Head, Plan, Response};
use base64::Engine;
use std::collections::BTreeMap;

pub enum GitOp {
    InfoRefs,
    UploadPack,
    ReceivePack,
}

/// `/<repo>[.git]/info/refs` and friends -> (repo name, operation).
pub fn git_route(path: &str) -> Option<(String, GitOp)> {
    let (rest, op) = if let Some(r) = path.strip_suffix("/info/refs") {
        (r, GitOp::InfoRefs)
    } else if let Some(r) = path.strip_suffix("/git-upload-pack") {
        (r, GitOp::UploadPack)
    } else if let Some(r) = path.strip_suffix("/git-receive-pack") {
        (r, GitOp::ReceivePack)
    } else {
        return None;
    };
    let name = rest.strip_prefix('/')?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    crate::config::valid_repo(name).then(|| (name.to_string(), op))
}

pub fn unauthorized(msg: &str) -> Response<App> {
    Response::text(401, msg).with(
        "www-authenticate",
        "Basic realm=\"depot\", charset=\"UTF-8\"",
    )
}

fn no_cache(r: Response<App>) -> Response<App> {
    r.with("cache-control", "no-cache, max-age=0, must-revalidate")
        .with("pragma", "no-cache")
        .with("expires", "Fri, 01 Jan 1980 00:00:00 GMT")
}

fn server_error(e: &str) -> Response<App> {
    eprintln!("[depot] error: {e}");
    Response::text(503, &format!("depot: {e}"))
}

impl App {
    /// Who is asking. Err: credentials were presented and are wrong.
    pub fn who(&mut self, head: &Head) -> Result<Who, ()> {
        let mut token: Option<String> = None;
        if let Some(a) = head.header("authorization") {
            if let Some(b) = a
                .strip_prefix("Basic ")
                .or_else(|| a.strip_prefix("basic "))
            {
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(b.trim())
                    .map_err(|_| ())?;
                let s = String::from_utf8(raw).map_err(|_| ())?;
                let (u, p) = s.split_once(':').unwrap_or((s.as_str(), ""));
                token = Some(if p.is_empty() {
                    u.to_string()
                } else {
                    p.to_string()
                });
            } else if let Some(t) = a.strip_prefix("Bearer ") {
                token = Some(t.trim().to_string());
            }
        }
        for h in ["x-api-key", "x-depot-token"] {
            if let Some(t) = head.header(h) {
                token = Some(t.trim().to_string());
            }
        }
        if let (Some(t), Some(sso)) = (head.header("x-sso-token"), &self.cfg.sso) {
            let claims = crate::sso::verify(sso, t.trim(), crate::app::now()).map_err(|_| ())?;
            return match self.cfg.user_for_account(&claims.sub) {
                Some(u) => Ok(Who::User(u.to_string())),
                None => Ok(Who::Anonymous),
            };
        }
        let Some(t) = token.filter(|t| !t.is_empty()) else {
            return Ok(Who::Anonymous);
        };
        if let Some(u) = self.cfg.user_for(&t) {
            return Ok(Who::User(u.to_string()));
        }
        if t.starts_with(crate::tokens::PREFIX) {
            if self.tokens.find(&t).is_none() {
                // minted moments ago (or elsewhere): look again, at most every few seconds
                if let Err(e) = self.tokens.refresh(&mut self.store, false) {
                    eprintln!("[depot] token book: {e}");
                }
            }
            if let Some(x) = self.tokens.find(&t) {
                return Ok(crate::tokens::who(x));
            }
        }
        Err(())
    }

    /// Readable repository id, or the response that refuses it.
    /// Readable repository id, or the response that refuses it. Access is
    /// judged from the registry alone, before anything is loaded: a caller
    /// who may not read learns nothing (anonymous: 401 whether or not the
    /// name exists; signed in: 404) and costs no index load.
    pub fn readable(&mut self, head: &Head, repo: &str) -> Result<(Who, String), Response<App>> {
        let who = self
            .who(head)
            .map_err(|_| unauthorized("bad credentials"))?;
        self.may_read(&who, repo)?;
        match self.open(repo).map_err(|e| server_error(&e))? {
            Some(id) => Ok((who, id)),
            None => Err(Response::text(404, "repository not found")),
        }
    }

    pub fn may_read(&mut self, who: &Who, repo: &str) -> Result<(), Response<App>> {
        let meta = self.meta(repo).map_err(|e| server_error(&e))?;
        let public = meta.as_ref().is_some_and(|m| m.public) || self.cfg.public_by_config(repo);
        if !can_read(&self.cfg, who, repo, public) {
            return Err(match who {
                Who::Anonymous => unauthorized("authentication required"),
                _ => Response::text(404, "repository not found"),
            });
        }
        if meta.is_none() {
            return Err(Response::text(404, "repository not found"));
        }
        Ok(())
    }

    fn info_refs(&mut self, head: &Head, repo: &str) -> Response<App> {
        let service = head.param("service").unwrap_or_default();
        match service.as_str() {
            "git-upload-pack" => {
                let (_, id) = match self.readable(head, repo) {
                    Ok(x) => x,
                    Err(r) => return r,
                };
                let reach = self.repos.get_mut(&id).unwrap().reach();
                let r = &self.repos[&id];
                let v2 = head
                    .header("git-protocol")
                    .is_some_and(|v| v.split(':').any(|p| p.trim() == "version=2"));
                let body = if v2 {
                    upload::advertise_v2()
                } else {
                    let mut b = Vec::new();
                    pkt::line(&mut b, "# service=git-upload-pack\n");
                    pkt::flush(&mut b);
                    b.extend_from_slice(&upload::advertise_v0_upload(&r.view(&reach)));
                    b
                };
                no_cache(
                    Response::new(200).bytes("application/x-git-upload-pack-advertisement", body),
                )
            }
            "git-receive-pack" => {
                let who = match self.who(head) {
                    Ok(w) => w,
                    Err(_) => return unauthorized("bad credentials"),
                };
                if who == Who::Anonymous {
                    return unauthorized("authentication required to push");
                }
                if !can_write(&self.cfg, &who, repo) {
                    return Response::text(403, &format!("{} may not push to {repo}", who.name()));
                }
                let id = match self.open(repo) {
                    Ok(x) => x,
                    Err(e) => return server_error(&e),
                };
                let mut b = Vec::new();
                pkt::line(&mut b, "# service=git-receive-pack\n");
                pkt::flush(&mut b);
                match id.and_then(|i| self.repos.get(&i)) {
                    Some(r) => b.extend_from_slice(&receive::advertise(
                        &r.view(&crate::git::walk::Bits::new(0)),
                    )),
                    None => {
                        let (ix, refs) = (Index::default(), BTreeMap::new());
                        let head_name = format!("refs/heads/{}", self.cfg.default_branch);
                        b.extend_from_slice(&receive::advertise(&View {
                            ix: &ix,
                            refs: &refs,
                            head: &head_name,
                            reach: &crate::git::walk::Bits::new(0),
                        }));
                    }
                }
                no_cache(
                    Response::new(200).bytes("application/x-git-receive-pack-advertisement", b),
                )
            }
            _ => Response::text(
                403,
                "this server speaks the smart HTTP protocol only (git 1.6.6 or newer)",
            ),
        }
    }

    fn upload_pack(&mut self, head: &Head, repo: &str, body: Vec<u8>) -> Response<App> {
        let (_, id) = match self.readable(head, repo) {
            Ok(x) => x,
            Err(r) => return r,
        };
        let reach = self.repos.get_mut(&id).unwrap().reach();
        let r = &self.repos[&id];
        let view = r.view(&reach);
        let v2 = head
            .header("git-protocol")
            .is_some_and(|v| v.split(':').any(|p| p.trim() == "version=2"));
        let ct = "application/x-git-upload-pack-result";
        let result = if v2 {
            match upload::parse_v2(&body) {
                Ok(req) => match req.command.as_str() {
                    "ls-refs" => {
                        return no_cache(
                            Response::new(200).bytes(ct, upload::ls_refs(&view, &req.args)),
                        )
                    }
                    "fetch" => upload::fetch_v2(&view, &req.args),
                    c => Err(format!("unknown command {c}")),
                },
                Err(e) => Err(e),
            }
        } else {
            upload::upload_v0(&view, &body)
        };
        match result {
            Ok(f) if f.gen.is_none() => no_cache(Response::new(200).bytes(ct, f.head)),
            Ok(f) => {
                let src = FetchSource {
                    repo_id: id,
                    repo: repo.to_string(),
                    fetch: f,
                    started: std::time::Instant::now(),
                    bytes: 0,
                };
                no_cache(Response::new(200).stream(ct, Box::new(src)))
            }
            Err(e) => {
                eprintln!("[depot] fetch {repo}: {e}");
                no_cache(Response::new(200).bytes(ct, upload::error_pkt(&e)))
            }
        }
    }
}

impl Handler for App {
    fn plan(&mut self, head: &Head) -> Plan<App> {
        if !self.ready {
            return Plan::Respond(
                Response::text(503, "depot is starting: storage is not reachable yet")
                    .with("retry-after", "10")
                    .with("cache-control", "no-store"),
            );
        }
        if let Some((repo, op)) = git_route(&head.path) {
            return match (op, head.method.as_str()) {
                (GitOp::InfoRefs, "GET") => Plan::Respond(self.info_refs(head, &repo)),
                // who may read is settled before a byte of the body is held
                (GitOp::UploadPack, "POST") => match self.readable(head, &repo) {
                    Ok(_) => Plan::Buffer(8 << 20),
                    Err(r) => Plan::Respond(r),
                },
                (GitOp::ReceivePack, "POST") => {
                    let who = match self.who(head) {
                        Ok(w) => w,
                        Err(_) => return Plan::Respond(unauthorized("bad credentials")),
                    };
                    if who == Who::Anonymous {
                        return Plan::Respond(unauthorized("authentication required to push"));
                    }
                    if !can_write(&self.cfg, &who, &repo) {
                        return Plan::Respond(Response::text(
                            403,
                            &format!("{} may not push to {repo}", who.name()),
                        ));
                    }
                    let limits = Limits {
                        max_pack: self.cfg.max_push,
                        max_object: self.cfg.max_object,
                    };
                    Plan::Stream(Box::new(PushSink::new(
                        &repo,
                        who,
                        limits,
                        self.push_budget.clone(),
                    )))
                }
                _ => Plan::Respond(Response::text(405, "method not allowed")),
            };
        }
        crate::api::plan(self, head)
    }

    fn handle(&mut self, head: &Head, body: Vec<u8>) -> Response<App> {
        if let Some((repo, GitOp::UploadPack)) = git_route(&head.path) {
            return self.upload_pack(head, &repo, body);
        }
        crate::api::handle(self, head, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let (n, op) = git_route("/enclave.git/info/refs").unwrap();
        assert_eq!(n, "enclave");
        assert!(matches!(op, GitOp::InfoRefs));
        assert_eq!(
            git_route("/EnclaveHost/enclave/git-upload-pack").unwrap().0,
            "EnclaveHost/enclave"
        );
        assert!(git_route("/a/b/c/info/refs").is_none());
        assert!(git_route("/info/refs").is_none());
        assert!(git_route("/api/repos").is_none());
    }
}
