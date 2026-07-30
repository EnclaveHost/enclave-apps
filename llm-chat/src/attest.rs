//! The hardware's own account of itself, for the attestation dialog.
//!
//! The dialog used to hand over a CLI install line. That is a fine way to
//! VERIFY and a poor way to SHOW: almost nobody installs a tool to read their
//! own SEV-SNP registers, so the registers went unseen. This route fetches the
//! platform's attestation record for THIS deployment and hands the page the
//! real thing - the launch measurement, the TLS key the quote binds, the GPU's
//! confidential-computing mode and the nonce it answered - so the popup shows
//! hardware instead of installation instructions.
//!
//! WHOSE WORD THIS IS. The party being verified cannot vouch for itself, and
//! nothing here pretends otherwise. Every field carries where it came from, the
//! signed document's own endpoint is handed to the page so the BROWSER can
//! re-fetch it over its own TLS connection (that endpoint reflects the
//! requesting origin, so it may), and the cryptographic check - quote signature
//! to the AMD root, measurement to the Sigstore-signed release - still belongs
//! to a verifier the user runs. What this buys is that the numbers are on
//! screen at all, and that a wrong one is visible rather than buried.
//!
//! Two hops, both public and unauthenticated:
//!   GET /v1/deployments/<8-hex label>         -> the full id, and which enclave
//!   GET /v1/deployments/<full id>/attestation -> quote, measurements, GPU report
//! The label comes from the request's own Host (<label>.app.enclave.host),
//! because the platform passes no deployment id into the guest environment: an
//! app learns its own identity from the hostname it was asked on, or not at all.

use crate::http::{self, HttpReq};

/// The platform API. Reachable from a deployment with egress; a deployment
/// without any outbound leg simply cannot show this, and says so.
const API: &str = "https://api.enclave.host";

/// The record hop is small; the attestation hop carries a GPU report and two
/// certificate chains, so it needs room without being unbounded.
const REC_MAX: usize = 64 * 1024;
const ATT_MAX: usize = 512 * 1024;

/// The deployment label in a hostname: `<label>.app.enclave.host`, where the
/// label is a hex PREFIX of the bytes32 deployment id (a full id is 64 chars and
/// DNS stops at 63). Anything else - a custom domain, localhost, an IP - yields
/// None, and the caller degrades instead of guessing an id.
pub fn label_of_host(host: &str) -> Option<String> {
    let h = host.split(':').next()?.trim().to_ascii_lowercase();
    let first = h.split('.').next()?;
    let hex = first.strip_prefix("0x").unwrap_or(first);
    let ok = (8..=64).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| hex.to_string())
}

/// The same label from ENCLAVE_HOSTS, which lists every hostname this
/// deployment answers on. This is the path that keeps a CUSTOM domain working:
/// the request arrives on example.com, which spells no deployment id, but the
/// platform-issued `<label>.app.enclave.host` is still in the list.
pub fn label_from_env() -> Option<String> {
    std::env::var("ENCLAVE_HOSTS")
        .ok()?
        .split(',')
        .filter_map(label_of_host)
        .next()
}

/// Fetch and distill this deployment's attestation. `host` is the request's own
/// Host header.
pub fn document(host: Option<&str>) -> Result<serde_json::Value, String> {
    let label = host
        .and_then(label_of_host)
        .or_else(label_from_env)
        .ok_or_else(|| {
            "this deployment cannot tell which deployment it is: the request did not arrive on a \
             <id>.app.enclave.host hostname and ENCLAVE_HOSTS names none, so there is no id to \
             ask the platform about"
                .to_string()
        })?;
    let rec = get_json(&format!("{API}/v1/deployments/0x{label}"), REC_MAX)?;
    let id = rec
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("the platform's deployment record carried no id")?
        .to_string();
    let att = get_json(&format!("{API}/v1/deployments/{id}/attestation"), ATT_MAX)?;
    Ok(distill(&label, &rec, &att))
}

fn get_json(url: &str, cap: usize) -> Result<serde_json::Value, String> {
    let r = http::request(
        HttpReq::get(url)
            .header("accept", b"application/json")
            .timeout(20)
            .max_bytes(cap),
    )?;
    if r.status != 200 {
        let body = String::from_utf8_lossy(&r.body);
        let hint = body.chars().take(200).collect::<String>();
        return Err(format!("{url} answered HTTP {} ({hint})", r.status));
    }
    if r.truncated {
        return Err(format!("{url} answered more than {cap} bytes"));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("{url} did not answer JSON: {e}"))
}

/// Everything the dialog renders, and nothing it does not: the two certificate
/// chains and the raw GPU report are kilobytes of base64 that no popup displays,
/// so they are reported as PRESENT with their size rather than shipped. The CPU
/// quote does ride along - it is 1.2 KB and the page parses the registers out of
/// it, which is the difference between showing an attestation and describing one.
fn distill(label: &str, rec: &serde_json::Value, att: &serde_json::Value) -> serde_json::Value {
    let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let vm = att.get("vm").cloned().unwrap_or(serde_json::Value::Null);
    let gpu = att.get("gpu").cloned().unwrap_or(serde_json::Value::Null);
    let ver = att.get("verification").cloned().unwrap_or(serde_json::Value::Null);
    let card = gpu
        .get("gpus")
        .and_then(|g| g.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let b64_len = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|x| x.len() / 4 * 3)
            .unwrap_or(0)
    };

    serde_json::json!({
        "deployment": {
            "id": s(rec, "id"),
            "label": label,
            "enclave": s(rec, "enclave"),
            "status": s(rec, "status"),
            "image": rec.get("image").and_then(|i| i.get("reference")).and_then(|r| r.as_str()),
            "gpuShare": rec.get("resources").and_then(|r| r.get("gpuShare")),
        },
        // where each number came from, said out loud: this app relayed all of
        // it, and the page is expected to re-fetch `endpoint` itself.
        "source": {
            "relayedBy": "this deployment, over its egress leg",
            "api": API,
            "generatedAt": s(att, "generatedAt"),
            "endpoint": ver.get("attestationEndpoint"),
            "repo": ver.get("repo"),
            "browserVerifier": s(att, "guideUrl"),
            "cli": ver.get("cli"),
            "how": ver.get("how"),
        },
        "tlsKeyFingerprint": s(att, "tlsKeyFingerprint"),
        "app": att.get("app"),
        "vm": {
            "technology": s(&vm, "technology"),
            "measurement": vm.get("measurements").and_then(|m| m.get("measurement")),
            "reportData": s(&vm, "reportData"),
            // the raw report, for the page's own parse
            "quote": s(&vm, "quote"),
        },
        "gpu": {
            "technology": s(&gpu, "technology"),
            "ccMode": s(&gpu, "ccMode"),
            "nonce": s(&gpu, "nonce"),
            "driverVersion": s(&gpu, "driverVersion"),
            "vbiosVersion": s(&card, "vbiosVersion"),
            "uuid": s(&card, "uuid"),
            "cards": gpu.get("gpus").and_then(|g| g.as_array()).map(|a| a.len()),
            "gpuShare": gpu.get("gpuShare"),
            "vramCapGb": gpu.get("vramCapGb"),
            "reportBytes": b64_len(&gpu, "report"),
            "certChainBytes": b64_len(&gpu, "certChain"),
            "verify": s(&gpu, "verify"),
        },
        "selfCheck": ver.get("selfCheck"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostname_spells_the_deployment_label() {
        assert_eq!(label_of_host("cc1f4f3f.app.enclave.host").unwrap(), "cc1f4f3f");
        assert_eq!(label_of_host("CC1F4F3F.app.enclave.host:443").unwrap(), "cc1f4f3f");
        // a longer prefix is legal (the platform resolves any unique one)
        assert_eq!(label_of_host("cc1f4f3fada6c3cd.app.enclave.host").unwrap(), "cc1f4f3fada6c3cd");
        // and everything that is not an id yields nothing rather than a guess
        assert!(label_of_host("chat.example.com").is_none());
        assert!(label_of_host("localhost:8080").is_none());
        assert!(label_of_host("127.0.0.1").is_none());
        assert!(label_of_host("cc1f4f3.app.enclave.host").is_none(), "7 hex is too short");
        assert!(label_of_host("").is_none());
    }

    #[test]
    fn a_custom_domain_falls_back_to_the_platform_hostname() {
        // ENCLAVE_HOSTS is how an app learns the names it answers on; the
        // platform-issued one is the only one that spells an id.
        std::env::set_var("ENCLAVE_HOSTS", "chat.example.com,cc1f4f3f.app.enclave.host");
        assert_eq!(label_from_env().unwrap(), "cc1f4f3f");
        std::env::set_var("ENCLAVE_HOSTS", "chat.example.com");
        assert!(label_from_env().is_none());
        std::env::remove_var("ENCLAVE_HOSTS");
        assert!(label_from_env().is_none());
    }

    /// The shape of the real thing, trimmed. Field names are load-bearing: the
    /// page reads exactly these, so a rename upstream must break a test here
    /// rather than empty a row in the dialog.
    fn fixtures() -> (serde_json::Value, serde_json::Value) {
        let rec = serde_json::json!({
            "id": "0xcc1f4f3fada6c3cd8ad0dc4ed03823470639ad2c59edf0a1e1be93ee20241352",
            "status": "running", "enclave": "kryptos",
            "image": {"reference": "catalog://0x06a4/110"},
            "resources": {"gpuShare": 0.5, "cpuShare": 0.08},
        });
        let att = serde_json::json!({
            "generatedAt": "2026-07-30T04:10:36.619Z",
            "guideUrl": "https://enclave.host/#attest",
            "tlsKeyFingerprint": "sha256:3b9dd23bb18df3f2740baf16aa75fa915b97436579ba42352170bdb334860050",
            "app": {"kind": "catalog", "coverage": "Baked into the attested enclave image."},
            "verification": {
                "repo": "EnclaveHost/enclave",
                "cli": "tinfoil attestation verify -e kryptos.enclave.containers.tinfoil.dev -r EnclaveHost/enclave",
                "how": "Fetch /.well-known/tinfoil-attestation from this origin over your OWN TLS connection...",
                "attestationEndpoint": "https://kryptos.enclave.containers.tinfoil.dev/.well-known/tinfoil-attestation",
                "selfCheck": {"result": "pass", "steps": {"verifyEnclave": "pass"},
                              "release": "sha256:6396a4", "note": "Run by the enclave itself."},
            },
            "vm": {"technology": "amd-sev-snp", "quote": "AwAAAA==",
                   "measurements": {"measurement": "2d67113d"},
                   "reportData": "3b9dd23b93a32c1c"},
            "gpu": {"technology": "nvidia-cc", "ccMode": "on", "nonce": "f65ef392",
                    "driverVersion": "580.126.20", "report": "AAAAAAAA", "certChain": "AAAA",
                    "gpuShare": 0.5, "vramCapGb": 70.2, "verify": "Check the report with NRAS.",
                    "gpus": [{"index": 0, "uuid": "GPU-04f10d07", "vbiosVersion": "96.00.D0.00.03"}]},
        });
        (rec, att)
    }

    #[test]
    fn the_dialog_gets_registers_not_prose() {
        let (rec, att) = fixtures();
        let d = distill("cc1f4f3f", &rec, &att);
        assert_eq!(d["vm"]["technology"], "amd-sev-snp");
        assert_eq!(d["vm"]["measurement"], "2d67113d");
        assert_eq!(d["vm"]["quote"], "AwAAAA==", "the raw quote must ride along for the page's parse");
        assert_eq!(d["gpu"]["ccMode"], "on");
        assert_eq!(d["gpu"]["uuid"], "GPU-04f10d07");
        assert_eq!(d["gpu"]["cards"], 1);
        assert_eq!(d["deployment"]["enclave"], "kryptos");
        assert_eq!(d["deployment"]["label"], "cc1f4f3f");
        assert_eq!(d["selfCheck"]["result"], "pass");
        assert_eq!(d["tlsKeyFingerprint"],
            "sha256:3b9dd23bb18df3f2740baf16aa75fa915b97436579ba42352170bdb334860050");
        // the page needs the endpoint to re-fetch the signed document ITSELF
        assert_eq!(d["source"]["endpoint"],
            "https://kryptos.enclave.containers.tinfoil.dev/.well-known/tinfoil-attestation");
        assert_eq!(d["source"]["repo"], "EnclaveHost/enclave");
        // and the kilobyte blobs are reported, not shipped
        assert_eq!(d["gpu"]["reportBytes"], 6);
        assert!(d["gpu"].get("report").is_none(), "the GPU report itself must not be inlined");
    }

    #[test]
    fn missing_upstream_fields_leave_holes_rather_than_lies() {
        // a CPU-only node has no gpu block, and an older platform may not carry
        // every field; the dialog must be able to render the gaps as gaps.
        let (rec, _) = fixtures();
        let thin = serde_json::json!({"vm": {"technology": "amd-sev-snp"}});
        let d = distill("cc1f4f3f", &rec, &thin);
        assert_eq!(d["vm"]["technology"], "amd-sev-snp");
        assert!(d["vm"]["measurement"].is_null());
        assert!(d["gpu"]["ccMode"].is_null());
        assert!(d["selfCheck"].is_null());
        assert_eq!(d["gpu"]["reportBytes"], 0);
    }
}
