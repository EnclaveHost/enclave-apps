//! ENCLAVE SESSIONS FOR CHATS: a chat that holds a wallet-approved session
//! key acts on Enclave (deploy, fund, stop, ...) without a wallet prompt per
//! transaction.
//!
//! The flow, end to end:
//!
//! 1. The browser makes a P-256 key for ONE chat and has the user's wallet
//!    approve it once at enclave.host/grant: a session in the user's
//!    SessionVault, with a USDC budget and an expiry.
//! 2. Every `POST /chat` of that chat carries the key in `x-enclave-session`
//!    (see HEADER for the format). This module reads it, only on a signed-in
//!    request, and only when the private scalar really is the key of the
//!    point it names (`parse`). The component keeps nothing between
//!    requests, so the key lives for exactly one request, in memory.
//! 3. When the model calls one of the session-capable builder tools
//!    (SESSION_TOOLS) on a server with sessions on (mcp.enclave.host by
//!    default), tools.rs adds `session: {vault, sid}` to the arguments, the
//!    server answers with session calls (EIP-712 digests), the key signs each
//!    digest here, and tools.rs sends them to the same server's
//!    `session_execute`. The model gets ONE result: what the builder said,
//!    plus what was executed.
//!
//! WHAT NEVER LEAVES: the private scalar. It is not logged, not echoed, not
//! in the prompt, not in any event. Only `vault`, `sid` (both public, on
//! chain) and the public point go to the server, which needs them to execute.
//! Nothing session-specific enters the system prompt or the tool schemas
//! either: the boot warm-up parks ONE prompt every signed-in chat shares, and
//! a per-chat line in it would make every chat miss that prefix.
//!
//! WHAT IS TRUSTED: the server's digests. The guest cannot judge an
//! ABI-encoded call on its own, and does not try: what a signature can do is
//! bounded on chain by the session's policy (its actions, environments,
//! budget and expiry), and the server is the platform's own MCP endpoint.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

/// The request header that carries a chat's session:
/// `base64url(no padding) of {"v":1,"vault":"0x<40 hex>","sid":"0x<64 hex>",
/// "x":"<b64url 32 bytes>","y":"<b64url 32 bytes>","d":"<b64url 32 bytes>"}`
/// - x, y and d exactly as WebCrypto exports the key as a JWK.
pub const HEADER: &str = "x-enclave-session";

/// The MCP host whose builders take a session without the config saying so.
pub const DEFAULT_HOST: &str = "mcp.enclave.host";

/// The builder tools that take `session: {vault, sid}` and then answer with
/// session calls to sign instead of wallet transactions (the server's
/// SESSION_CAPABLE set). Matched on the name the SERVER knows a tool by.
pub const SESSION_TOOLS: [&str; 10] = [
    "plan_deploy",
    "build_fund",
    "build_stop",
    "build_resume",
    "build_upgrade",
    "build_resize",
    "build_set_max_rate",
    "build_refund",
    "build_set_config",
    "build_publish",
];

/// The server tool that sends signed session calls (the relay pays the gas).
pub const EXECUTE_TOOL: &str = "session_execute";

/// Appended to a builder's RESULT (never the prompt) on a turn without a
/// session: the wallet transactions it returned cannot be signed here.
pub const NO_SESSION_NOTE: &str =
    "This chat has no Enclave session, so these transactions were not \
sent and can't be signed here. Ask the user to give this chat an Enclave session (chat menu → \
Enclave session), then call the tool again.";

/// The first line of a result whose calls this chat's session executed.
pub const EXECUTED_NOTE: &str =
    "executed through this chat's Enclave session (no wallet signature)";

/// The longest header accepted. The real one is ~330 characters.
const MAX_HEADER: usize = 2048;

/// A chat's session, read from the request. No Debug, no Clone, no Serialize:
/// the private key must not be printable or copyable by accident.
pub struct ChatSession {
    /// the SessionVault, 0x + 40 lowercase hex
    pub vault: String,
    /// the session id, 0x + 64 lowercase hex
    pub sid: String,
    x: [u8; 32],
    y: [u8; 32],
    key: SigningKey,
}

/// The header's JSON. Unknown fields are ignored, so the browser can add
/// some (a label) without breaking older guests.
#[derive(serde::Deserialize)]
struct Wire {
    v: u32,
    vault: String,
    sid: String,
    x: String,
    y: String,
    d: String,
}

impl ChatSession {
    /// The session in a header value. The error says what was wrong in
    /// terms of field names only - never the value - so it can be logged.
    pub fn parse(header: &str) -> Result<ChatSession, String> {
        let h = header.trim();
        if h.is_empty() {
            return Err("the header is empty".into());
        }
        if h.len() > MAX_HEADER {
            return Err(format!("the header is longer than {MAX_HEADER} characters"));
        }
        // the format says no padding; a padded value is the same bytes
        let json = URL_SAFE_NO_PAD
            .decode(h.trim_end_matches('='))
            .map_err(|_| "the header is not base64url".to_string())?;
        let w: Wire = serde_json::from_slice(&json).map_err(|_| {
            "the header is not the session JSON {v, vault, sid, x, y, d}".to_string()
        })?;
        if w.v != 1 {
            return Err(format!("unknown session format v{}", w.v));
        }
        let vault = hex_field(&w.vault, 20).ok_or("vault is not 0x + 40 hex")?;
        let sid = hex_field(&w.sid, 32).ok_or("sid is not 0x + 64 hex")?;
        let x = b64_32(&w.x).ok_or("x is not 32 bytes of base64url")?;
        let y = b64_32(&w.y).ok_or("y is not 32 bytes of base64url")?;
        let d = b64_32(&w.d).ok_or("d is not 32 bytes of base64url")?;
        let key = SigningKey::from_bytes(&p256::FieldBytes::from(d))
            .map_err(|_| "d is not a P-256 private key".to_string())?;
        // the point the browser registered must be THIS key's, or every
        // signature would be refused on chain after the server had quoted it
        let point = key.verifying_key().to_encoded_point(false);
        if point.x().map(|v| &v[..]) != Some(&x[..]) || point.y().map(|v| &v[..]) != Some(&y[..]) {
            return Err("d is not the private key of (x, y)".into());
        }
        Ok(ChatSession {
            vault,
            sid,
            x,
            y,
            key,
        })
    }

    /// `{vault, sid}`, the argument a session-capable builder takes.
    pub fn arg(&self) -> serde_json::Value {
        serde_json::json!({ "vault": self.vault, "sid": self.sid })
    }

    /// The public point as session_execute takes it: `{x, y}`, 0x + 64 hex.
    pub fn public_key(&self) -> serde_json::Value {
        serde_json::json!({ "x": hex0x(&self.x), "y": hex0x(&self.y) })
    }

    /// Sign one session call's 32-byte digest: ECDSA P-256 over SHA-256 of
    /// those bytes, RFC6979 - what WebCrypto's sign({name:'ECDSA',
    /// hash:'SHA-256'}, key, digest) computes and what the vault checks with
    /// P256VERIFY(sha256(digest), r, s, x, y). 64 bytes, r || s.
    pub fn sign(&self, digest: &[u8; 32]) -> [u8; 64] {
        let sig: Signature = self.key.sign(digest);
        let mut out = [0u8; 64];
        out.copy_from_slice(&sig.to_bytes());
        out
    }

    /// Is this the session a server's answer names? Addresses compare
    /// without case (the server checksums them).
    fn is(&self, vault: Option<&str>, sid: Option<&str>) -> bool {
        vault.is_some_and(|v| v.eq_ignore_ascii_case(&self.vault))
            && sid.is_some_and(|s| s.eq_ignore_ascii_case(&self.sid))
    }
}

/// A request's session: Ok(None) when it sent no header, Ok(Some) when the
/// header is a valid session on a SIGNED-IN request, Err(why) otherwise -
/// the caller logs the reason (words only, never the value) and carries on
/// with no session, exactly as if none had been sent.
pub fn accept(header: Option<&str>, signed_in: bool) -> Result<Option<ChatSession>, String> {
    let Some(h) = header.filter(|h| !h.trim().is_empty()) else {
        return Ok(None);
    };
    if !signed_in {
        return Err("the request is not signed in".into());
    }
    ChatSession::parse(h).map(Some)
}

/// `0x` + exactly `n` bytes of hex, lowercased.
fn hex_field(s: &str, n: usize) -> Option<String> {
    let h = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))?;
    (h.len() == n * 2 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("0x{}", h.to_ascii_lowercase()))
}

fn b64_32(s: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD
        .decode(s.trim().trim_end_matches('='))
        .ok()?
        .try_into()
        .ok()
}

fn hex0x(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let h = s.trim().strip_prefix("0x")?;
    // from_str_radix alone would take a "+f" pair
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Does a call to the server tool `remote` take a session?
pub fn is_session_tool(remote: &str) -> bool {
    SESSION_TOOLS.contains(&remote)
}

/// Sessions on for an MCP entry by default: https to DEFAULT_HOST.
pub fn default_on(url: &str) -> bool {
    match crate::http::split_url(url.trim()) {
        Ok((scheme, authority, _)) => {
            let a = authority.to_ascii_lowercase();
            scheme == "https" && a.strip_suffix(":443").unwrap_or(&a) == DEFAULT_HOST
        }
        Err(_) => false,
    }
}

/// The model's arguments with this chat's session added, or None when they
/// stay as they are: the model named a session itself (never overridden),
/// or the arguments are not an object.
pub fn with_session_arg(args: &serde_json::Value, s: &ChatSession) -> Option<serde_json::Value> {
    let o = args.as_object()?;
    if o.get("session").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let mut o = o.clone();
    o.insert("session".into(), s.arg());
    Some(serde_json::Value::Object(o))
}

/// A tools/call result as an object: structuredContent, else the text part
/// read as JSON (a server that sends only text).
pub fn answer_object(r: &serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    if let Some(o) = r.get("structuredContent").and_then(|s| s.as_object()) {
        return Some(o.clone());
    }
    let text: String = r
        .get("content")?
        .as_array()?
        .iter()
        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    match serde_json::from_str::<serde_json::Value>(text.trim()) {
        Ok(serde_json::Value::Object(o)) => Some(o),
        _ => None,
    }
}

/// Does a builder's answer carry anything to sign: wallet `transactions`,
/// or session `calls`? A builder with nothing to do says so with both empty.
pub fn has_work(answer: &serde_json::Map<String, serde_json::Value>) -> bool {
    ["transactions", "calls"].iter().any(|k| {
        answer
            .get(*k)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    })
}

/// What to do with a builder's answer on a turn that has a session.
pub enum Plan {
    /// not session calls (an ordinary answer, or `calls` empty): as it is
    Keep,
    /// session calls for a session that is not this chat's (the model named
    /// another one): left unsigned, with this note
    Foreign(String),
    /// signed: the arguments for session_execute
    Execute(serde_json::Value),
}

/// Read a builder's answer and, when it is session calls for this chat's
/// session, sign every one. Err: the answer claims to be session calls but
/// one of them cannot be signed (no 32-byte digest) - nothing is sent then.
pub fn plan(
    answer: &serde_json::Map<String, serde_json::Value>,
    s: &ChatSession,
) -> Result<Plan, String> {
    if answer.get("via").and_then(|v| v.as_str()) != Some("session") {
        return Ok(Plan::Keep);
    }
    let calls = match answer.get("calls").and_then(|c| c.as_array()) {
        Some(c) if !c.is_empty() => c,
        _ => return Ok(Plan::Keep),
    };
    let (vault, sid) = (
        answer.get("vault").and_then(|v| v.as_str()),
        answer.get("sid").and_then(|v| v.as_str()),
    );
    if !s.is(vault, sid) {
        return Ok(Plan::Foreign(
            "These session calls are for a session that is not this chat's Enclave session, so \
             they were not signed or sent. Call the tool again without a `session` argument to \
             act through this chat's session."
                .into(),
        ));
    }
    let mut signed = Vec::with_capacity(calls.len());
    for (i, c) in calls.iter().enumerate() {
        let Some(mut c) = c.as_object().cloned() else {
            return Err(format!(
                "the server's session call {i} is not an object; nothing was signed or sent"
            ));
        };
        let Some(digest) = c.get("digest").and_then(|d| d.as_str()).and_then(unhex32) else {
            return Err(format!(
                "the server's session call {i} has no 32-byte digest to sign; nothing was signed or sent"
            ));
        };
        c.insert(
            "signature".into(),
            serde_json::Value::String(hex0x(&s.sign(&digest))),
        );
        signed.push(serde_json::Value::Object(c));
    }
    Ok(Plan::Execute(serde_json::json!({
        "vault": s.vault,
        "sid": s.sid,
        "publicKey": s.public_key(),
        "calls": signed,
    })))
}

/// The ONE result the model gets for a builder whose calls were executed:
/// the builder's informative fields (not the calls, digests or signing
/// instructions it no longer needs), plus what session_execute reported -
/// `executed` (a txHash per call), `createdId` for a deploy, its `next`.
pub fn executed_result(
    answer: &serde_json::Map<String, serde_json::Value>,
    executed: Option<serde_json::Map<String, serde_json::Value>>,
    executed_text: &str,
) -> String {
    let mut info = answer.clone();
    for k in ["calls", "sign", "next", "digest", "digestSha256"] {
        info.remove(k);
    }
    match executed {
        Some(e) => info.extend(e),
        None => {
            info.insert(
                "executed".into(),
                serde_json::Value::String(executed_text.trim().to_string()),
            );
        }
    }
    format!("{EXECUTED_NOTE}\n{}", serde_json::Value::Object(info))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    // A throwaway key, generated once with node:crypto
    // (generateKeyPairSync('ec', {namedCurve:'P-256'}), exported as a JWK),
    // with a 32-byte digest and node's own signature of it:
    // crypto.sign('sha256', digest, {key, dsaEncoding:'ieee-p1363'}).
    const X: &str = "jcHy4AhMBlQwSb0H6o1G1nZHOC2vtV5gDO3VfGZO6A4";
    const Y: &str = "4FeW_gzMZp_s5FW9p6H3NJ-12G1YysYCsME0wR8cTM4";
    pub(crate) const D: &str = "SZ3drLL8SEldSgzb0SBYH7RQGKQPwrmykXj5ZGqmKz0";
    pub(crate) const DIGEST: &str =
        "0x64ee6d07720e7c1fdaedfd207b01e4b3808b1745b8e27e5688ed23bebe8c0725";
    const NODE_SIG: &str = "0x36f1f0d7c65aa8c3fce91c3459a36a5ccd6dd6c9f78ae98088ee8a2c38dd775a\
                            7240b5ed0aaf6a3358f643f4ef3883d64bde809261b1e730c01617c9baac179e";
    // this module's RFC6979 signature of DIGEST with D, checked once with
    // node 2026-10-07: crypto.verify('sha256', digest, {key: jwk,
    // dsaEncoding: 'ieee-p1363'}, sig) and WebCrypto's subtle.verify(
    // {name:'ECDSA', hash:'SHA-256'}, key, sig, digest) both true (and both
    // false for the digest with one bit flipped)
    const OUR_SIG: &str = "0x3879b479795a641f3e0073215df593b29e9e97ae43ef3038d2e20505c5399215\
                           914cba22aa0c4390eaec4e1e8982025fd3da92ef0ccf607bc9f653322c5ea029";

    pub(crate) const VAULT: &str = "0xB794C4DD00000000000000000000000000000345";
    pub(crate) const SID: &str =
        "0x00000000000000000000000000000000000000000000000000000000000000a1";

    fn header(v: serde_json::Value) -> String {
        URL_SAFE_NO_PAD.encode(v.to_string())
    }

    fn good() -> serde_json::Value {
        serde_json::json!({ "v": 1, "vault": VAULT, "sid": SID, "x": X, "y": Y, "d": D })
    }

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
        let h = s.strip_prefix("0x").unwrap();
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    }

    pub(crate) fn test_session() -> ChatSession {
        ChatSession::parse(&header(good())).unwrap()
    }

    #[test]
    fn a_good_header_parses() {
        let s = test_session();
        assert_eq!(s.vault, VAULT.to_ascii_lowercase());
        assert_eq!(s.sid, SID);
        assert_eq!(
            s.arg(),
            serde_json::json!({ "vault": VAULT.to_ascii_lowercase(), "sid": SID })
        );
        let pk = s.public_key();
        assert_eq!(
            unhex(pk["x"].as_str().unwrap()),
            URL_SAFE_NO_PAD.decode(X).unwrap()
        );
        assert_eq!(
            unhex(pk["y"].as_str().unwrap()),
            URL_SAFE_NO_PAD.decode(Y).unwrap()
        );
        // padded, surrounded by whitespace, extra fields: the same session
        let mut extra = good();
        extra["label"] = "my chat".into();
        let padded = format!("  {}==  ", header(extra));
        assert_eq!(ChatSession::parse(&padded).unwrap().sid, SID);
    }

    #[test]
    fn a_bad_header_is_refused_without_echoing_it() {
        let with = |k: &str, v: serde_json::Value| {
            let mut j = good();
            j[k] = v;
            ChatSession::parse(&header(j))
        };
        // d of another key: (x, y) are not its point
        let other = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let e = with("d", other.clone().into()).err().unwrap();
        assert_eq!(e, "d is not the private key of (x, y)");
        // and a d that is no scalar at all
        assert!(with("d", URL_SAFE_NO_PAD.encode([0u8; 32]).into()).is_err());
        assert!(with("d", URL_SAFE_NO_PAD.encode([0xffu8; 32]).into()).is_err());
        // shapes
        assert!(with("v", 2.into()).is_err());
        assert!(with("vault", "0x1234".into()).is_err());
        assert!(with("vault", "b794c4dd00000000000000000000000000000345".into()).is_err());
        assert!(with(
            "sid",
            "0xzz00000000000000000000000000000000000000000000000000000000000000".into()
        )
        .is_err());
        assert!(with("x", URL_SAFE_NO_PAD.encode([1u8; 31]).into()).is_err());
        assert!(with("y", "not base64!".into()).is_err());
        assert!(with("d", serde_json::Value::Null).is_err());
        // garbage
        for g in [
            "",
            "   ",
            "%%%",
            "eyJ2IjoxfQ",
            &"A".repeat(5000),
            &URL_SAFE_NO_PAD.encode("[1,2]"),
        ] {
            assert!(ChatSession::parse(g).is_err(), "{g:.40}");
        }
        // no message carries key material
        for (k, v) in [("d", other.as_str()), ("x", "zz"), ("vault", "0x12")] {
            let e = with(k, v.into()).err().unwrap();
            assert!(!e.contains(v) && !e.contains(D), "{e}");
        }
    }

    #[test]
    fn only_a_signed_in_request_brings_a_session() {
        let h = header(good());
        assert!(accept(Some(&h), true).unwrap().is_some());
        assert_eq!(
            accept(Some(&h), false).err().unwrap(),
            "the request is not signed in"
        );
        assert!(accept(None, false).unwrap().is_none());
        assert!(accept(Some("  "), true).unwrap().is_none());
        // garbage on a signed-in request is a reason to log, not a session
        assert!(accept(Some("garbage"), true).is_err());
    }

    #[test]
    fn signatures_verify_both_ways_against_node() {
        let s = test_session();
        let digest: [u8; 32] = unhex(DIGEST).try_into().unwrap();
        let vk = VerifyingKey::from_sec1_bytes(
            &[
                &[4u8][..],
                &URL_SAFE_NO_PAD.decode(X).unwrap(),
                &URL_SAFE_NO_PAD.decode(Y).unwrap(),
            ]
            .concat(),
        )
        .unwrap();
        // node's signature (randomized) verifies here: same curve, same
        // SHA-256-over-the-32-bytes rule, same r || s encoding
        let node = Signature::from_slice(&unhex(NODE_SIG)).unwrap();
        vk.verify(&digest, &node)
            .expect("node's signature verifies");
        // ours verifies here, and it is deterministic (RFC6979), so the
        // value node accepted is pinned
        let ours = s.sign(&digest);
        vk.verify(&digest, &Signature::from_slice(&ours).unwrap())
            .expect("our signature verifies");
        assert_eq!(
            s.sign(&digest),
            ours,
            "RFC6979: the same digest signs the same"
        );
        assert_eq!(hex0x(&ours), OUR_SIG);
        // a different digest does not verify under it
        let mut other = digest;
        other[0] ^= 1;
        assert!(vk
            .verify(&other, &Signature::from_slice(&ours).unwrap())
            .is_err());
    }

    #[test]
    fn the_session_argument_is_added_but_never_overrides() {
        let s = test_session();
        let a = with_session_arg(&serde_json::json!({ "id": "0x01" }), &s).unwrap();
        assert_eq!(a["session"], s.arg());
        assert_eq!(a["id"], "0x01");
        // a null session is no session
        let a = with_session_arg(&serde_json::json!({ "session": null }), &s).unwrap();
        assert_eq!(a["session"], s.arg());
        // the model's own is kept
        assert!(with_session_arg(
            &serde_json::json!({ "session": { "vault": "0x1", "sid": "0x2" } }),
            &s
        )
        .is_none());
        assert!(with_session_arg(&serde_json::json!("x"), &s).is_none());
        // which tools, which hosts
        assert!(is_session_tool("plan_deploy") && is_session_tool("build_publish"));
        assert!(!is_session_tool("session_execute") && !is_session_tool("get_deployment"));
        assert!(
            default_on("https://mcp.enclave.host")
                && default_on("https://MCP.enclave.host:443/mcp")
        );
        assert!(
            !default_on("http://mcp.enclave.host")
                && !default_on("https://mcp.enclave.host.evil.example")
        );
        assert!(!default_on("https://evil.example/mcp.enclave.host") && !default_on("not a url"));
    }

    #[test]
    fn a_session_answer_is_signed_only_for_this_chats_session() {
        let s = test_session();
        let call = serde_json::json!({ "action": 6, "actionName": "deploy.setActive", "nonce": "3",
            "fee": "1000", "deadline": "1790000000", "args": "0xab", "digest": DIGEST, "digestSha256": "0x00" });
        let answer = |vault: &str| {
            serde_json::json!({ "via": "session", "vault": vault, "sid": SID, "owner": "0x0b2d",
                "calls": [call.clone()], "sign": "...", "next": "...", "id": "0x9eb4" })
            .as_object()
            .cloned()
            .unwrap()
        };
        let Plan::Execute(x) = plan(&answer(VAULT), &s).unwrap() else {
            panic!("not signed")
        };
        assert_eq!(x["vault"], s.vault);
        assert_eq!(x["sid"], s.sid);
        assert_eq!(x["publicKey"], s.public_key());
        let c = &x["calls"][0];
        assert_eq!(c["nonce"], "3");
        assert_eq!(c["args"], "0xab");
        let digest: [u8; 32] = unhex(DIGEST).try_into().unwrap();
        assert_eq!(c["signature"], hex0x(&s.sign(&digest)));
        // the server checksums the vault: still this session
        assert!(matches!(
            plan(&answer(&VAULT.to_ascii_lowercase()), &s).unwrap(),
            Plan::Execute(_)
        ));
        // another session's calls are not signed
        assert!(matches!(
            plan(&answer("0x0000000000000000000000000000000000000001"), &s).unwrap(),
            Plan::Foreign(_)
        ));
        // not session calls, or none: kept as they are
        let plain = serde_json::json!({ "transactions": [] })
            .as_object()
            .cloned()
            .unwrap();
        assert!(matches!(plan(&plain, &s).unwrap(), Plan::Keep));
        let mut none = answer(VAULT);
        none.insert("calls".into(), serde_json::json!([]));
        assert!(matches!(plan(&none, &s).unwrap(), Plan::Keep));
        // a call with no digest stops everything
        let mut bad = answer(VAULT);
        bad.insert(
            "calls".into(),
            serde_json::json!([call, { "action": 1, "digest": "0x12" }]),
        );
        assert!(plan(&bad, &s).err().unwrap().contains("call 1"));
    }

    #[test]
    fn the_executed_result_keeps_the_builders_facts_and_drops_the_signing() {
        let answer = serde_json::json!({ "via": "session", "vault": VAULT, "sid": SID, "owner": "0x0b2d",
            "app": "hello-world", "calls": [{ "digest": DIGEST }], "sign": "how", "next": "then" });
        let exec = serde_json::json!({ "executed": [{ "call": 0, "txHash": "0xfeed" }], "createdId": "0xc0de",
            "next": "claim_hint" });
        let text = executed_result(answer.as_object().unwrap(), exec.as_object().cloned(), "");
        let (first, rest) = text.split_once('\n').unwrap();
        assert_eq!(first, EXECUTED_NOTE);
        let j: serde_json::Value = serde_json::from_str(rest).unwrap();
        assert_eq!(j["app"], "hello-world");
        assert_eq!(j["executed"][0]["txHash"], "0xfeed");
        assert_eq!(j["createdId"], "0xc0de");
        assert_eq!(
            j["next"], "claim_hint",
            "the execute's next, not the builder's"
        );
        assert!(j.get("calls").is_none() && j.get("sign").is_none(), "{j}");
        assert!(!rest.contains(DIGEST));
        // a session_execute that answered in prose still reaches the model
        let text = executed_result(answer.as_object().unwrap(), None, " sent 0xfeed ");
        assert!(text.contains("\"executed\":\"sent 0xfeed\""), "{text}");
    }

    #[test]
    fn an_answer_is_read_from_structured_content_or_its_text() {
        let o = serde_json::json!({ "via": "session" });
        let both = serde_json::json!({ "content": [{ "type": "text", "text": "summary" }], "structuredContent": o });
        assert_eq!(answer_object(&both).unwrap()["via"], "session");
        let text = serde_json::json!({ "content": [{ "type": "text", "text": o.to_string() }] });
        assert_eq!(answer_object(&text).unwrap()["via"], "session");
        let prose = serde_json::json!({ "content": [{ "type": "text", "text": "done" }] });
        assert!(answer_object(&prose).is_none());
    }
}
