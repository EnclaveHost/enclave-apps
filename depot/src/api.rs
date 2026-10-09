//! The JSON API and the web view: list and administer repositories, and
//! browse refs, history and files. Repository-scoped calls name the
//! repository in `?repo=` (names may contain one `/`).
//!
//! Raw file bytes are served as plain text or an octet stream under a
//! sandboxing CSP, never as HTML: repository content must not run script
//! on this origin.

use crate::app::{now, App, Repo};
use crate::config::{can_read, is_admin, valid_repo, Who};
use crate::git::object::{parse_commit, tree_entries, MODE_GITLINK, MODE_TREE};
use crate::git::{Kind, Oid};
use crate::push::register;
use crate::routes::unauthorized;
use crate::serve::{Head, Plan, Response};
use crate::store::Saved;
use serde_json::{json, Value};

const UI: &str = include_str!("ui.html");
const SSO_RETURN: &str = include_str!("sso-return.html");
const UI_JS: &str = include_str!("ui.js");
const SSO_RETURN_JS: &str = include_str!("sso-return.js");
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'unsafe-inline' 'self'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'";

fn bad(msg: &str) -> Response<App> {
    Response::json(400, &json!({ "error": msg }))
}
fn not_found() -> Response<App> {
    Response::json(404, &json!({ "error": "not found" }))
}
fn fail(e: &str) -> Response<App> {
    eprintln!("[depot] api error: {e}");
    Response::json(503, &json!({ "error": e }))
}

pub fn plan(app: &mut App, head: &Head) -> Plan<App> {
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", _) | ("HEAD", _) => Plan::Respond(get(app, head)),
        ("POST" | "PATCH" | "DELETE", p) if p.starts_with("/api/") => Plan::Buffer(1 << 20),
        _ => Plan::Respond(Response::text(405, "method not allowed")),
    }
}

pub fn handle(app: &mut App, head: &Head, body: Vec<u8>) -> Response<App> {
    let who = match app.who(head) {
        Ok(w) => w,
        Err(_) => return unauthorized("bad credentials"),
    };
    // browsers send Origin on cross-site writes; this API takes same-origin or none
    if let (Some(o), Some(h)) = (head.header("origin"), head.header("host")) {
        let host = o.split("://").nth(1).unwrap_or("");
        if host != h {
            return Response::json(
                403,
                &json!({ "error": "cross-origin requests are refused" }),
            );
        }
    }
    let v: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return bad("body must be JSON"),
        }
    };
    match (head.method.as_str(), head.path.as_str()) {
        ("POST", "/api/repos") => create(app, &who, &v),
        ("PATCH", "/api/repo") => patch(app, &who, head, &v),
        ("DELETE", "/api/repo") => delete(app, &who, head),
        ("POST", "/api/tokens") => mint(app, &who, &v),
        ("DELETE", "/api/tokens") => {
            if !is_admin(&app.cfg, &who) {
                return Response::json(403, &json!({ "error": "admin only" }));
            }
            let Some(id) = head.param("id") else {
                return bad("id required");
            };
            match app.tokens.revoke(&mut app.store, &id) {
                Ok(true) => Response::json(200, &json!({ "revoked": id })),
                Ok(false) => not_found(),
                Err(e) => fail(&e),
            }
        }
        ("POST", "/api/maintenance") => {
            if !is_admin(&app.cfg, &who) {
                return Response::json(403, &json!({ "error": "admin only" }));
            }
            let Some(repo) = head.param("repo") else {
                return bad("repo required");
            };
            match crate::maint::request(app, &repo) {
                Ok(v) => Response::json(200, &v),
                Err(e) => fail(&e),
            }
        }
        _ => not_found(),
    }
}

fn get(app: &mut App, head: &Head) -> Response<App> {
    match head.path.as_str() {
        "/" | "/index.html" => {
            let html = UI.replace("__TITLE__", &html_escape(&app.cfg.title));
            return Response::new(200)
                .with("cache-control", "no-store")
                .with("x-frame-options", "DENY")
                .with("content-security-policy", CSP)
                .with("referrer-policy", "no-referrer")
                .bytes("text/html; charset=utf-8", html);
        }
        "/sso-return" => {
            return Response::new(200)
                .with("cache-control", "no-store")
                .with("content-security-policy", CSP)
                .with("referrer-policy", "no-referrer")
                .bytes("text/html; charset=utf-8", SSO_RETURN);
        }
        "/app.js" | "/sso-return.js" => {
            return Response::new(200)
                .with("cache-control", "no-cache")
                .with("x-content-type-options", "nosniff")
                .bytes(
                    "application/javascript; charset=utf-8",
                    if head.path == "/app.js" {
                        UI_JS
                    } else {
                        SSO_RETURN_JS
                    },
                );
        }
        "/ping" => return Response::text(200, "ok"),
        "/favicon.ico" => return Response::new(204),
        _ => {}
    }
    if !head.path.starts_with("/api/") {
        return Response::text(404, "not found (git URLs look like /<repo>.git)");
    }
    let who = match app.who(head) {
        Ok(w) => w,
        Err(_) => return unauthorized("bad credentials"),
    };
    match head.path.as_str() {
        "/api/whoami" => Response::json(
            200,
            &json!({ "user": who.name(), "signed_in": who != Who::Anonymous, "admin": is_admin(&app.cfg, &who),
                     "sso": app.cfg.sso.as_ref().map(|s| json!({"audience": s.audience, "authorize_url": s.authorize_url})) }),
        ),
        "/api/status" => status(app, &who),
        "/api/tokens" => {
            if !is_admin(&app.cfg, &who) {
                return Response::json(403, &json!({ "error": "admin only" }));
            }
            if let Err(e) = app.tokens.refresh(&mut app.store, true) {
                return fail(&e);
            }
            let list: Vec<Value> = app
                .tokens
                .book
                .tokens
                .iter()
                .map(|t| json!({"id": t.id, "user": t.user, "admin": t.admin, "read": t.read, "write": t.write,
                                "created": t.created, "expires": t.expires, "note": t.note, "by": t.by}))
                .collect();
            Response::json(200, &json!({ "tokens": list }))
        }
        "/api/repos" => list(app, &who),
        "/api/repo" | "/api/log" | "/api/tree" | "/api/raw" | "/api/commit" => {
            let Some(repo) = head.param("repo") else {
                return bad("repo required");
            };
            if let Err(r) = app.may_read(&who, &repo) {
                return r;
            }
            let id = match app.open(&repo) {
                Ok(Some(id)) => id,
                Ok(None) => return not_found(),
                Err(e) => return fail(&e),
            };
            match head.path.as_str() {
                "/api/repo" => info(app, &id),
                "/api/log" => log(app, &id, head),
                "/api/tree" => tree(app, &id, head),
                "/api/raw" => raw(app, &id, head),
                _ => commit(app, &id, head),
            }
        }
        _ => not_found(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn status(app: &App, who: &Who) -> Response<App> {
    let mut v = json!({
        "app": "depot",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": app.started.elapsed().as_secs(),
        "repositories": app.reg.repos.len(),
    });
    if is_admin(&app.cfg, who) {
        let (hits, misses, bytes) = app.store.cache_stats();
        v["loaded"] = json!(app
            .repos
            .values()
            .map(|r| json!({"name": r.name, "objects": r.ix.len(), "packs": r.m.packs.len(), "bytes": r.bytes(), "refs": r.refs.len()}))
            .collect::<Vec<_>>());
        v["cache"] = json!({"hits": hits, "misses": misses, "bytes": bytes});
        v["storage"] = json!({"calls": app.store.s3.calls, "connects": app.store.s3.connects(), "bytes_in": app.store.s3.bytes_in, "bytes_out": app.store.s3.bytes_out});
        v["traffic"] = json!({"fetches": app.stats.fetches, "pushes": app.stats.pushes, "push_bytes": app.stats.push_bytes, "sent_bytes": app.stats.sent_bytes, "failures": app.stats.failures, "push_bytes_held": app.push_budget.held()});
        v["maintenance"] = crate::maint::status(app);
        v["webhooks"] = crate::hooks::status(app);
    }
    Response::json(200, &v)
}

fn list(app: &mut App, who: &Who) -> Response<App> {
    if let Err(e) = app.refresh_registry(false) {
        return fail(&e);
    }
    let names: Vec<(String, crate::store::RepoMeta)> = app
        .reg
        .repos
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = Vec::new();
    for (name, meta) in names {
        let public = meta.public || app.cfg.public_by_config(&name);
        if !can_read(&app.cfg, who, &name, public) {
            continue;
        }
        let loaded = app.repos.get(&meta.id);
        out.push(json!({
            "name": name,
            "public": public,
            "description": meta.description,
            "created": meta.created,
            "refs": loaded.map(|r| r.refs.len()),
            "head": loaded.map(|r| r.m.head.clone()),
            "bytes": loaded.map(|r| r.bytes()),
            "updated": loaded.map(|r| r.m.updated),
        }));
    }
    Response::json(200, &json!({ "repos": out }))
}

fn create(app: &mut App, who: &Who, v: &Value) -> Response<App> {
    if !is_admin(&app.cfg, who) {
        return Response::json(403, &json!({ "error": "admin only" }));
    }
    let Some(name) = v["name"].as_str() else {
        return bad("name required");
    };
    if !valid_repo(name) {
        return bad("invalid repository name");
    }
    match app.meta(name) {
        Ok(Some(_)) => return Response::json(409, &json!({ "error": "exists" })),
        Ok(None) => {}
        Err(e) => return fail(&e),
    }
    if let Err(e) = register(app, name, &crate::seal::random_id()) {
        return fail(&e);
    }
    if v.get("public").is_some() || v.get("description").is_some() {
        if let Err(e) = update_meta(app, name, v) {
            return fail(&e);
        }
    }
    Response::json(201, &json!({ "created": name }))
}

fn mint(app: &mut App, who: &Who, v: &Value) -> Response<App> {
    if !is_admin(&app.cfg, who) {
        return Response::json(403, &json!({ "error": "admin only" }));
    }
    let user = v["user"].as_str().unwrap_or("").to_string();
    if !crate::config::valid_user(&user) {
        return bad("user: 1-64 of [A-Za-z0-9-_.@]");
    }
    let pats = |k: &str| -> Result<Vec<String>, String> {
        let Some(a) = v.get(k) else {
            return Ok(Vec::new());
        };
        let a = a
            .as_array()
            .ok_or(format!("{k} must be a list of repository patterns"))?;
        a.iter()
            .map(|x| {
                x.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 200)
                    .map(String::from)
                    .ok_or(format!("bad {k} pattern"))
            })
            .collect()
    };
    let (read, write) = match (pats("read"), pats("write")) {
        (Ok(r), Ok(w)) => (r, w),
        (Err(e), _) | (_, Err(e)) => return bad(&e),
    };
    let admin = v["admin"].as_bool().unwrap_or(false);
    if !admin && read.is_empty() && write.is_empty() {
        return bad("give the token read and/or write patterns (or admin)");
    }
    let expires = v["expires_days"]
        .as_u64()
        .filter(|d| *d > 0 && *d <= 3650)
        .map(|d| now() + d * 86400);
    let rec = crate::tokens::Token {
        id: String::new(),
        hash: String::new(),
        user,
        admin,
        read,
        write,
        created: 0,
        expires,
        note: v["note"].as_str().unwrap_or("").chars().take(200).collect(),
        by: who.name().to_string(),
    };
    match app.tokens.mint(&mut app.store, rec) {
        Ok((secret, rec)) => Response::json(
            201,
            &json!({ "token": secret, "id": rec.id, "user": rec.user, "expires": rec.expires,
                     "note": "shown once: store it now" }),
        ),
        Err(e) => fail(&e),
    }
}

fn update_meta(app: &mut App, name: &str, v: &Value) -> Result<(), String> {
    for _ in 0..8 {
        app.refresh_registry(true)?;
        let mut r = app.reg.clone();
        let m = r.repos.get_mut(name).ok_or("no such repository")?;
        if let Some(p) = v["public"].as_bool() {
            m.public = p;
        }
        if let Some(d) = v["description"].as_str() {
            m.description = d.chars().take(500).collect();
        }
        let etag = app.reg_etag.clone();
        if let Saved::Ok(t) = app.store.save_registry(&mut r, etag.as_deref())? {
            app.reg = r;
            app.reg_etag = Some(t);
            return Ok(());
        }
    }
    Err("registry busy; try again".into())
}

fn patch(app: &mut App, who: &Who, head: &Head, v: &Value) -> Response<App> {
    if !is_admin(&app.cfg, who) {
        return Response::json(403, &json!({ "error": "admin only" }));
    }
    let Some(repo) = head.param("repo") else {
        return bad("repo required");
    };
    let id = match app.open(&repo) {
        Ok(Some(id)) => id,
        Ok(None) => return not_found(),
        Err(e) => return fail(&e),
    };
    if v.get("public").is_some() || v.get("description").is_some() {
        if let Err(e) = update_meta(app, &repo, v) {
            return fail(&e);
        }
    }
    if let Some(h) = v["head"].as_str() {
        let target = if h.starts_with("refs/") {
            h.to_string()
        } else {
            format!("refs/heads/{h}")
        };
        if !crate::git::valid_refname(&target) {
            return bad("invalid head");
        }
        for attempt in 0..8 {
            if attempt > 0 {
                if let Err(e) = app.revalidate(&id, true) {
                    return fail(&e);
                }
            }
            // (a registry refresh above may have dropped a repository deleted elsewhere)
            let Some(r) = app.repos.get(&id) else {
                return not_found();
            };
            if !r.refs.contains_key(&target) {
                return bad("head must name an existing branch");
            }
            let mut m = r.m.clone();
            m.head = target.clone();
            let etag = r.etag.clone();
            match app.store.save_manifest(&id, &mut m, etag.as_deref(), now()) {
                Ok(Saved::Ok(t)) => {
                    if let Some(r) = app.repos.get_mut(&id) {
                        r.m = m;
                        r.etag = Some(t);
                    }
                    break;
                }
                Ok(Saved::Conflict) => continue,
                Err(e) => return fail(&e),
            }
        }
    }
    info(app, &id)
}

fn delete(app: &mut App, who: &Who, head: &Head) -> Response<App> {
    if !is_admin(&app.cfg, who) {
        return Response::json(403, &json!({ "error": "admin only" }));
    }
    let Some(repo) = head.param("repo") else {
        return bad("repo required");
    };
    if head.param("confirm").as_deref() != Some(repo.as_str()) {
        return bad("pass confirm=<repository name> to delete it");
    }
    let mut removed = None;
    for _ in 0..8 {
        if let Err(e) = app.refresh_registry(true) {
            return fail(&e);
        }
        let mut r = app.reg.clone();
        let Some(meta) = r.repos.remove(&repo) else {
            return not_found();
        };
        let etag = app.reg_etag.clone();
        match app.store.save_registry(&mut r, etag.as_deref()) {
            Ok(Saved::Ok(t)) => {
                app.reg = r;
                app.reg_etag = Some(t);
                removed = Some(meta.id);
                break;
            }
            Ok(Saved::Conflict) => continue,
            Err(e) => return fail(&e),
        }
    }
    let Some(id) = removed else {
        return fail("registry busy; try again");
    };
    app.repos.remove(&id);
    match app.store.delete_repo_objects(&id) {
        Ok(n) => Response::json(200, &json!({ "deleted": repo, "objects": n })),
        Err(e) => Response::json(200, &json!({ "deleted": repo, "cleanup_error": e })),
    }
}

fn info(app: &App, id: &str) -> Response<App> {
    let Some(r) = app.repos.get(id) else {
        return not_found();
    };
    let refs: serde_json::Map<String, Value> = r
        .refs
        .iter()
        .map(|(k, v)| (k.clone(), json!(v.hex())))
        .collect();
    Response::json(
        200,
        &json!({
            "name": r.name,
            "head": r.m.head,
            "refs": refs,
            "packs": r.m.packs.len(),
            "objects": r.ix.len(),
            "bytes": r.bytes(),
            "updated": r.m.updated,
            "revision": r.m.rev,
            "public": app.is_public(&r.name),
        }),
    )
}

/// The commit `rev` names, if a reader may see it: a ref (or the commit a
/// tag peels to), or an object id that some ref reaches. Ids of objects no
/// ref reaches (force-pushed away, deleted branches) are not served.
fn start_commit(app: &mut App, id: &str, rev: &str) -> Option<Oid> {
    let r = app.repos.get_mut(id)?;
    let c = resolve(r, rev).and_then(|o| to_commit(r, o))?;
    readable(r, &c).then_some(c)
}

fn readable(r: &mut Repo, o: &Oid) -> bool {
    let reach = r.reach();
    r.ix.lookup(o).is_some_and(|i| reach.get(i))
}

fn resolve(r: &Repo, rev: &str) -> Option<Oid> {
    let rev = if rev.is_empty() || rev == "HEAD" {
        r.m.head.as_str()
    } else {
        rev
    };
    for c in [
        rev.to_string(),
        format!("refs/heads/{rev}"),
        format!("refs/tags/{rev}"),
    ] {
        if let Some(o) = r.refs.get(&c) {
            return Some(*o);
        }
    }
    Oid::from_hex(rev).filter(|o| r.ix.lookup(o).is_some())
}

/// Peel a ref value to its commit.
fn to_commit(r: &Repo, o: Oid) -> Option<Oid> {
    let i = r.ix.lookup(&o)?;
    let p = r.ix.peel(i)?;
    (r.ix.obj(p).kind == Kind::Commit).then(|| r.ix.oid(p))
}

fn log(app: &mut App, id: &str, head: &Head) -> Response<App> {
    let n: usize = head
        .param("n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
        .min(200);
    let skip: usize = head
        .param("skip")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        .min(100_000);
    let rev = head.param("ref").unwrap_or_default();
    let Some(start) = start_commit(app, id, &rev) else {
        return not_found();
    };
    let r = &app.repos[id];
    // newest-first over the commit graph (committer time)
    let ix = &r.ix;
    let mut heap = std::collections::BinaryHeap::new();
    let mut seen = std::collections::HashSet::new();
    let s = ix.lookup(&start).unwrap();
    heap.push((ix.obj(s).time, s));
    seen.insert(s);
    let mut picked = Vec::new();
    let mut k = 0;
    while let Some((_, c)) = heap.pop() {
        if k >= skip {
            picked.push(ix.oid(c));
            if picked.len() >= n {
                break;
            }
        }
        k += 1;
        for &p in ix.kids(c).iter().skip(1) {
            if p != crate::git::repo::NONE && seen.insert(p) {
                heap.push((ix.obj(p).time, p));
            }
        }
    }
    let mut out = Vec::new();
    for o in picked {
        match app.read_object(id, &o) {
            Ok(Some((_, data))) => {
                if let Ok(c) = parse_commit(&data) {
                    out.push(json!({
                        "id": o.hex(),
                        "parents": c.parents.iter().map(|p| p.hex()).collect::<Vec<_>>(),
                        "author": c.author,
                        "committer": c.committer,
                        "subject": c.message.lines().next().unwrap_or(""),
                        "time": crate::git::object::ident_time(c.committer.as_bytes()),
                    }));
                }
            }
            Ok(None) => {}
            Err(e) => return fail(&e),
        }
    }
    Response::json(200, &json!({ "commits": out, "start": start.hex() }))
}

/// Walk `path` from the tree of `rev`: the object it names and its mode.
fn lookup_path(
    app: &mut App,
    id: &str,
    rev: &str,
    path: &str,
) -> Result<Option<(Oid, u32, Oid)>, String> {
    let Some(c) = start_commit(app, id, rev) else {
        return Ok(None);
    };
    let r = &app.repos[id];
    let ci = r.ix.lookup(&c).unwrap();
    let Some(&t) =
        r.ix.kids(ci)
            .first()
            .filter(|&&t| t != crate::git::repo::NONE)
    else {
        return Ok(None);
    };
    let mut cur = r.ix.oid(t);
    let mut mode = MODE_TREE;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if mode != MODE_TREE {
            return Ok(None);
        }
        let Some((_, data)) = app.read_object(id, &cur)? else {
            return Ok(None);
        };
        let mut found = None;
        for e in tree_entries(&data) {
            let e = e?;
            if e.name == seg.as_bytes() {
                found = Some((e.oid, e.mode));
                break;
            }
        }
        let Some((o, m)) = found else { return Ok(None) };
        cur = o;
        mode = m;
    }
    Ok(Some((cur, mode, c)))
}

fn tree(app: &mut App, id: &str, head: &Head) -> Response<App> {
    let rev = head.param("ref").unwrap_or_default();
    let path = head.param("path").unwrap_or_default();
    let (oid, mode, commit) = match lookup_path(app, id, &rev, &path) {
        Ok(Some(x)) => x,
        Ok(None) => return not_found(),
        Err(e) => return fail(&e),
    };
    if mode != MODE_TREE {
        return Response::json(
            200,
            &json!({ "type": "blob", "id": oid.hex(), "commit": commit.hex() }),
        );
    }
    let data = match app.read_object(id, &oid) {
        Ok(Some((_, d))) => d,
        Ok(None) => return not_found(),
        Err(e) => return fail(&e),
    };
    let r = &app.repos[id];
    let mut entries = Vec::new();
    for e in tree_entries(&data) {
        let Ok(e) = e else { break };
        let kind = match e.mode {
            MODE_TREE => "tree",
            MODE_GITLINK => "commit",
            0o120000 => "symlink",
            _ => "blob",
        };
        let size = r.ix.lookup(&e.oid).map(|i| r.ix.obj(i).size);
        entries.push(json!({ "name": String::from_utf8_lossy(e.name), "type": kind, "mode": format!("{:o}", e.mode), "id": e.oid.hex(), "size": size }));
    }
    entries.sort_by_key(|e| {
        (
            e["type"] != "tree",
            e["name"].as_str().unwrap_or("").to_lowercase(),
        )
    });
    Response::json(
        200,
        &json!({ "type": "tree", "id": oid.hex(), "commit": commit.hex(), "entries": entries }),
    )
}

fn raw(app: &mut App, id: &str, head: &Head) -> Response<App> {
    let rev = head.param("ref").unwrap_or_default();
    let path = head.param("path").unwrap_or_default();
    let oid = match lookup_path(app, id, &rev, &path) {
        Ok(Some((o, mode, _))) if mode != MODE_TREE && mode != MODE_GITLINK => o,
        Ok(_) => return not_found(),
        Err(e) => return fail(&e),
    };
    // a file goes out from memory whole; past this size, clone the repository
    let size = app.repos[id]
        .ix
        .lookup(&oid)
        .map(|i| app.repos[id].ix.obj(i).size)
        .unwrap_or(0);
    if size > RAW_MAX {
        return Response::json(
            413,
            &json!({ "error": format!("{} MiB is too large to serve here; clone the repository", size >> 20) }),
        );
    }
    let mut charge = app.raw_budget.charge();
    if charge.set(size, RAW_BUDGET).is_err() {
        return Response::json(
            503,
            &json!({ "error": "busy serving other files; try again" }),
        );
    }
    let data = match app.read_object(id, &oid) {
        Ok(Some((_, d))) => d,
        Ok(None) => return not_found(),
        Err(e) => return fail(&e),
    };
    let text = std::str::from_utf8(&data).is_ok() && !data.contains(&0);
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or("file")
        .replace(['"', '\\', '\r', '\n'], "_");
    let disp = if head.param("download").is_some() {
        "attachment"
    } else {
        "inline"
    };
    Response::new(200)
        .with("x-content-type-options", "nosniff")
        .with("content-security-policy", "sandbox; default-src 'none'")
        .with("cache-control", "private, max-age=60")
        .with(
            "content-disposition",
            &format!("{disp}; filename=\"{name}\""),
        )
        .stream(
            if text {
                "text/plain; charset=utf-8"
            } else {
                "application/octet-stream"
            },
            Box::new(RawSource {
                data,
                pos: 0,
                _charge: charge,
            }),
        )
}

const RAW_MAX: u64 = 32 << 20;
const RAW_BUDGET: u64 = 256 << 20;

/// A file's bytes, drained into the connection as it can take them (no
/// copy of the whole into the write buffer); its charge on the shared raw
/// budget ends when the response does.
struct RawSource {
    data: std::rc::Rc<Vec<u8>>,
    pos: usize,
    _charge: crate::push::Charge,
}

impl crate::serve::Source<App> for RawSource {
    fn pull(&mut self, _app: &mut App, out: &mut Vec<u8>) -> Result<bool, String> {
        let end = (self.pos + (256 << 10)).min(self.data.len());
        out.extend_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(self.pos == self.data.len())
    }
}

fn commit(app: &mut App, id: &str, head: &Head) -> Response<App> {
    let Some(o) = head.param("id").and_then(|s| Oid::from_hex(&s)) else {
        return bad("id required");
    };
    if !app.repos.get_mut(id).is_some_and(|r| readable(r, &o)) {
        return not_found();
    }
    let data = match app.read_object(id, &o) {
        Ok(Some((Kind::Commit, d))) => d,
        Ok(_) => return not_found(),
        Err(e) => return fail(&e),
    };
    let Ok(c) = parse_commit(&data) else {
        return fail("malformed commit");
    };
    // the paths this commit changed against its first parent (one tree level at a time)
    let changes = match c.parents.first() {
        Some(p) => diff_paths(app, id, p, &o).unwrap_or_default(),
        None => Vec::new(),
    };
    Response::json(
        200,
        &json!({
            "id": o.hex(), "tree": c.tree.hex(),
            "parents": c.parents.iter().map(|p| p.hex()).collect::<Vec<_>>(),
            "author": c.author, "committer": c.committer, "message": c.message,
            "changes": changes,
        }),
    )
}

/// Changed paths between two commits' trees (bounded).
fn diff_paths(app: &mut App, id: &str, a: &Oid, b: &Oid) -> Result<Vec<Value>, String> {
    let tree_of = |app: &mut App, c: &Oid| -> Result<Option<Oid>, String> {
        Ok(match app.read_object(id, c)? {
            Some((Kind::Commit, d)) => Some(parse_commit(&d)?.tree),
            _ => None,
        })
    };
    let (Some(ta), Some(tb)) = (tree_of(app, a)?, tree_of(app, b)?) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), Some(ta), Some(tb))];
    let entries = |app: &mut App, t: Option<Oid>| -> Result<Vec<(String, u32, Oid)>, String> {
        let Some(t) = t else { return Ok(Vec::new()) };
        let Some((_, d)) = app.read_object(id, &t)? else {
            return Ok(Vec::new());
        };
        tree_entries(&d)
            .map(|e| e.map(|e| (String::from_utf8_lossy(e.name).into_owned(), e.mode, e.oid)))
            .collect()
    };
    while let Some((prefix, x, y)) = stack.pop() {
        if out.len() >= 500 {
            break;
        }
        let ex = entries(app, x)?;
        let ey = entries(app, y)?;
        let mut names: Vec<&String> = ex
            .iter()
            .map(|e| &e.0)
            .chain(ey.iter().map(|e| &e.0))
            .collect();
        names.sort();
        names.dedup();
        for n in names {
            let a = ex.iter().find(|e| &e.0 == n);
            let b = ey.iter().find(|e| &e.0 == n);
            let path = if prefix.is_empty() {
                n.clone()
            } else {
                format!("{prefix}/{n}")
            };
            match (a, b) {
                (Some(a), Some(b)) if a.2 == b.2 && a.1 == b.1 => {}
                (Some(a), Some(b)) if a.1 == MODE_TREE && b.1 == MODE_TREE => {
                    stack.push((path, Some(a.2), Some(b.2)))
                }
                (None, Some(b)) if b.1 == MODE_TREE => stack.push((path, None, Some(b.2))),
                (Some(a), None) if a.1 == MODE_TREE => stack.push((path, Some(a.2), None)),
                (None, Some(_)) => out.push(json!({"path": path, "change": "added"})),
                (Some(_), None) => out.push(json!({"path": path, "change": "deleted"})),
                _ => out.push(json!({"path": path, "change": "modified"})),
            }
        }
    }
    out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(out)
}
