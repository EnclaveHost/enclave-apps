//! Contract-wallet (EIP-1271) signature checks by AGREEING owner-pinned chain readers.
//!
//! VERBATIM copy of the shared, reviewed implementation the suite's apps carry in their `wallet.rs`
//! (eyesoff-ai, jot, depot, api-mcp-adapter - identical files, sha256 4619d6bb1fcf37d8e647c975756e57523fa5426de9b20b6f90bfde455425ed3f),
//! lines 122-150, 249-296, 297-405, 474-476, 840-866, 1048-1295: ContractCfg (+ parse), RpcUrl, hex, ChainReaders/HttpReaders,
//! and the EIP-1271 / ERC-6492 section ending in check_1271. Nothing here is edited; keep it byte-identical to that
//! source when the shared file changes (test `copied_verbatim_from_the_shared_wallet_module` pins the hash of this
//! body). The upload route REFUSES ERC-6492 (counterfactual) signatures before calling check_1271 (account_upload.rs),
//! so the 6492 half is present only to stay a faithful copy.
#![allow(dead_code)]

/// `wallet.contract_wallets`: how a contract wallet's signature is checked.
/// Explicitly pinned JSON-RPC readers (distinct hosts). A verdict counts only
/// when at least `quorum` of them return the IDENTICAL snapshot - block
/// number, block hash, block time and isValidSignature's answer at that hash -
/// and no other snapshot also reaches `quorum` (check_1271). One reader can
/// neither approve nor veto; `quorum` colluding readers can do either, so
/// choose more than half of `rpcs`, from operators you do not expect to collude.
///
/// ELAPSED TIME. Readers are asked one after another: up to `rpcs.len()` head
/// requests, then up to two requests (header, call) per remaining reader, each
/// bounded by `timeout_secs`. A check can therefore take up to about
/// 3 x readers x timeout_secs (7 readers at 15 s: ~5 minutes). Freshness and
/// sign-in expiry are judged AFTER that, so readers that hang until their
/// timeout can push an honest check past `max_age_secs` (or past the sign-in's
/// expiry) and refuse it. Slow readers are tolerated only within that budget:
/// keep 3 x readers x timeout_secs well under `max_age_secs`.
#[derive(Clone, Debug, PartialEq)]
pub struct ContractCfg {
    pub rpcs: Vec<String>,
    pub quorum: usize,
    /// blocks behind the lowest reported head the check is made at
    pub confirmations: u64,
    /// how old that block may be, seconds
    pub max_age_secs: u64,
    /// per reader request
    pub timeout_secs: u64,
    pub max_bytes: usize,
}


impl ContractCfg {
    pub fn parse(c: &serde_json::Value) -> Result<ContractCfg, String> {
        let rpcs: Vec<String> = c
            .get("rpcs")
            .and_then(|r| r.as_array())
            .ok_or("wallet.contract_wallets.rpcs must be a list of JSON-RPC URLs")?
            .iter()
            .map(|u| u.as_str().map(|u| u.trim().to_string()).ok_or("wallet.contract_wallets.rpcs entries must be strings".to_string()))
            .collect::<Result<_, _>>()?;
        if !(2..=7).contains(&rpcs.len()) {
            return Err("wallet.contract_wallets.rpcs must name 2 to 7 readers".into());
        }
        // Each URL is parsed strictly, and each must be a distinct READER: one
        // public host is one vote however its port, path, case or trailing dot
        // is spelled. Diagnostics name the entry by index and canonical host,
        // never by URL - a path or query may carry an API key.
        let mut seen: Vec<(String, usize)> = Vec::new();
        for (i, u) in rpcs.iter().enumerate() {
            let r = RpcUrl::parse(u).map_err(|why| format!("wallet.contract_wallets.rpcs[{i}] is not a usable reader URL: {why}"))?;
            if let Some((_, j)) = seen.iter().find(|(id, _)| *id == r.reader_id()) {
                return Err(format!(
                    "wallet.contract_wallets.rpcs[{i}] is the same reader as rpcs[{j}] ({}): each reader must be a distinct host",
                    r.reader_id()
                ));
            }
            seen.push((r.reader_id(), i));
        }
        let num = |k: &str, d: u64| -> Result<u64, String> {
            match c.get(k) {
                None | Some(serde_json::Value::Null) => Ok(d),
                Some(v) => v.as_u64().ok_or(format!("wallet.contract_wallets.{k} must be a non-negative integer")),
            }
        };
        let quorum = num("quorum", 2)? as usize;
        if quorum < 2 || quorum > rpcs.len() {
            return Err("wallet.contract_wallets.quorum must be at least 2 and at most the number of readers".into());
        }
        Ok(ContractCfg {
            rpcs,
            quorum,
            confirmations: num("confirmations", 2)?.min(64),
            max_age_secs: num("max_age_secs", 120)?.clamp(10, 3600),
            timeout_secs: num("timeout_secs", 5)?.clamp(1, 15),
            max_bytes: (num("max_bytes", 65_536)? as usize).clamp(4096, 1 << 20),
        })
    }
}


/// A pinned reader URL, parsed strictly. Grammar accepted, nothing else:
///
///   ("https" | "http") "://" host [":" port] ["/" path]
///   host = DNS name (letters, digits, hyphens; labels 1..63, total <= 253,
///          one optional trailing dot) | dotted-quad IPv4 | "[" IPv6 "]"
///   port = 1..65535 in decimal, no sign
///   path = letters, digits and "-._~/" only
///
/// No credentials (`@`), query (`?`), fragment (`#`), percent-escapes,
/// whitespace or non-ASCII anywhere. A name whose last label is numeric must
/// be a valid dotted quad (so `127.1` or `0x7f.0.0.1` are refused, not
/// guessed at). Plain http only on loopback, a local node under test.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcUrl {
    pub https: bool,
    /// canonical: lowercase, no trailing dot; IPv4 dotted quad; IPv6 in
    /// brackets in its compressed form (an IPv4-mapped one as its IPv4)
    pub host: String,
    pub port: u16,
    pub loopback: bool,
}

impl RpcUrl {
    pub fn parse(u: &str) -> Result<RpcUrl, &'static str> {
        if !u.is_ascii() || u.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err("only plain ASCII, no spaces");
        }
        if u.contains(['@', '?', '#', '%', '\\']) {
            return Err("no credentials, query, fragment or escapes");
        }
        let (https, rest) = if let Some(r) = u.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = u.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("the scheme must be https:// (or http:// on loopback)");
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b)) {
            return Err("the path may hold only letters, digits and -._~/");
        }
        let (host_part, port_part) = if let Some(v6) = authority.strip_prefix('[') {
            let (inside, after) = v6.split_once(']').ok_or("an IPv6 host needs its closing bracket")?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or("junk after the IPv6 host")?),
            };
            let ip: std::net::Ipv6Addr = inside.parse().map_err(|_| "not an IPv6 address")?;
            let host = match ip.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => format!("[{ip}]"),
            };
            (host, port)
        } else {
            let (h, p) = match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            let h = h.to_ascii_lowercase();
            let h = h.strip_suffix('.').unwrap_or(&h).to_string();
            if h.is_empty() || h.len() > 253 {
                return Err("no host");
            }
            let labels: Vec<&str> = h.split('.').collect();
            let numeric_tail = labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()));
            if numeric_tail {
                let ip: std::net::Ipv4Addr = h.parse().map_err(|_| "a numeric host must be a dotted-quad IPv4 address")?;
                (ip.to_string(), p)
            } else {
                for l in &labels {
                    let ok = (1..=63).contains(&l.len())
                        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !l.starts_with('-')
                        && !l.ends_with('-');
                    if !ok {
                        return Err("not a valid host name");
                    }
                }
                (h, p)
            }
        };
        let port = match port_part {
            None => if https { 443 } else { 80 },
            Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
                p.parse::<u16>().ok().filter(|n| *n > 0).ok_or("the port must be 1..65535")?
            }
            Some(_) => return Err("the port must be 1..65535"),
        };
        let loopback = matches!(host_part.as_str(), "localhost" | "127.0.0.1" | "[::1]");
        if !https && !loopback {
            return Err("plain http is only for a loopback test node");
        }
        Ok(RpcUrl { https, host: host_part, port, loopback })
    }

    /// Who answers: one public HOST is one reader whatever its port or path;
    /// on loopback (a test rig, where every reader is the same machine) the
    /// port tells readers apart, and the three loopback spellings are one.
    pub fn reader_id(&self) -> String {
        if self.loopback {
            format!("loopback port {}", self.port)
        } else {
            self.host.clone()
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How a reader is reached: POST `body` (JSON-RPC) to `url`, within
/// `timeout_secs`, reading at most `max_bytes`. Each app supplies its own
/// outbound HTTP; this file makes no call itself.
pub type RpcPost<'a> = &'a mut dyn ChainReaders;

/// The pinned readers as the app reaches them, and the app's clock. The clock
/// is asked again AFTER the reads, so a slow round of readers cannot carry a
/// block or a sign-in past its freshness: the decision is made at decision
/// time, not at the time the request arrived.
pub trait ChainReaders {
    /// POST `body` (JSON-RPC) to `url` within `timeout_secs`, reading at most `max_bytes`
    fn post(&mut self, url: &str, body: &[u8], timeout_secs: u64, max_bytes: usize) -> Result<Vec<u8>, String>;
    /// unix seconds, now
    fn now(&self) -> u64;
}

/// An app's outbound HTTP function as ChainReaders, on the system clock.
pub struct HttpReaders<F: FnMut(&str, &[u8], u64, usize) -> Result<Vec<u8>, String>>(pub F);

impl<F: FnMut(&str, &[u8], u64, usize) -> Result<Vec<u8>, String>> ChainReaders for HttpReaders<F> {
    fn post(&mut self, url: &str, body: &[u8], timeout_secs: u64, max_bytes: usize) -> Result<Vec<u8>, String> {
        (self.0)(url, body, timeout_secs, max_bytes)
    }
    fn now(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

// ---- EIP-1271: contract wallets, by agreeing pinned readers ------------------

/// isValidSignature(bytes32,bytes) and its success value
const EIP1271_MAGIC: [u8; 4] = [0x16, 0x26, 0xba, 0x7e];

/// ERC-6492: the 32 bytes a counterfactual (not yet deployed) wallet's
/// signature ends in, after abi.encode(address factory, bytes factoryCalldata,
/// bytes signature).
pub const ERC6492_MAGIC: [u8; 32] = [0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92,
                                     0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92, 0x64, 0x92];
/// ERC-6492's off-chain validator (`ValidateSigOffchain`, constructor
/// (address signer, bytes32 hash, bytes signature)): run as a DEPLOYLESS
/// eth_call it deploys the wallet through its factory call if it has no code,
/// asks isValidSignature, and returns one byte, 0x01 or 0x00. Byte-exact copy
/// of viem 2.53.1 `erc6492SignatureValidatorByteCode`, 1684 bytes, sha256 of
/// this hex text 037d6b69e53bae264a9a752be534c6373b3f829fb606456f184d2ba841de6ea4,
/// keccak-256 041473019f1c0ec0ea1e4e47621014f0be087857c75b5cd71e7df4fa962d57b9 (pinned by
/// a test). Nothing is deployed: an eth_call is a simulation.
pub const ERC6492_VALIDATOR: &str = "0x608060405234801561001057600080fd5b5060405161069438038061069483398101604081905261002f9161051e565b600061003c848484610048565b9050806000526001601ff35b60007f64926492649264926492649264926492649264926492649264926492649264926100748361040c565b036101e7576000606080848060200190518101906100929190610577565b60405192955090935091506000906001600160a01b038516906100b69085906105dd565b6000604051808303816000865af19150503d80600081146100f3576040519150601f19603f3d011682016040523d82523d6000602084013e6100f8565b606091505b50509050876001600160a01b03163b60000361016057806101605760405162461bcd60e51b815260206004820152601e60248201527f5369676e617475726556616c696461746f723a206465706c6f796d656e74000060448201526064015b60405180910390fd5b604051630b135d3f60e11b808252906001600160a01b038a1690631626ba7e90610190908b9087906004016105f9565b602060405180830381865afa1580156101ad573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906101d19190610633565b6001600160e01b03191614945050505050610405565b6001600160a01b0384163b1561027a57604051630b135d3f60e11b808252906001600160a01b03861690631626ba7e9061022790879087906004016105f9565b602060405180830381865afa158015610244573d6000803e3d6000fd5b505050506040513d601f19601f820116820180604052508101906102689190610633565b6001600160e01b031916149050610405565b81516041146102df5760405162461bcd60e51b815260206004820152603a602482015260008051602061067483398151915260448201527f3a20696e76616c6964207369676e6174757265206c656e6774680000000000006064820152608401610157565b6102e7610425565b5060208201516040808401518451859392600091859190811061030c5761030c61065d565b016020015160f81c9050601b811480159061032b57508060ff16601c14155b1561038c5760405162461bcd60e51b815260206004820152603b602482015260008051602061067483398151915260448201527f3a20696e76616c6964207369676e617475726520762076616c756500000000006064820152608401610157565b60408051600081526020810180835289905260ff83169181019190915260608101849052608081018390526001600160a01b0389169060019060a0016020604051602081039080840390855afa1580156103ea573d6000803e3d6000fd5b505050602060405103516001600160a01b0316149450505050505b9392505050565b600060208251101561041d57600080fd5b508051015190565b60405180606001604052806003906020820280368337509192915050565b6001600160a01b038116811461045857600080fd5b50565b634e487b7160e01b600052604160045260246000fd5b60005b8381101561048c578181015183820152602001610474565b50506000910152565b600082601f8301126104a657600080fd5b81516001600160401b038111156104bf576104bf61045b565b604051601f8201601f19908116603f011681016001600160401b03811182821017156104ed576104ed61045b565b60405281815283820160200185101561050557600080fd5b610516826020830160208701610471565b949350505050565b60008060006060848603121561053357600080fd5b835161053e81610443565b6020850151604086015191945092506001600160401b0381111561056157600080fd5b61056d86828701610495565b9150509250925092565b60008060006060848603121561058c57600080fd5b835161059781610443565b60208501519093506001600160401b038111156105b357600080fd5b6105bf86828701610495565b604086015190935090506001600160401b0381111561056157600080fd5b600082516105ef818460208701610471565b9190910192915050565b828152604060208201526000825180604084015261061e816060850160208701610471565b601f01601f1916919091016060019392505050565b60006020828403121561064557600080fd5b81516001600160e01b03198116811461040557600080fd5b634e487b7160e01b600052603260045260246000fdfe5369676e617475726556616c696461746f72237265636f7665725369676e6572";

/// Is `sig` an ERC-6492 wrapper, and a well-formed one? The suffix, then the
/// strict ABI shape abi.encode(address, bytes, bytes): an address word, two
/// in-range offsets, lengths that fit, and a non-empty inner signature. Err for
/// a suffix over a malformed body: that is refused, never guessed at.
fn erc6492_parts(sig: &[u8]) -> Result<Option<(String, usize, usize)>, String> {
    let Some(body) = sig.strip_suffix(&ERC6492_MAGIC[..]) else { return Ok(None) };
    let bad = || "contract wallet: malformed EIP-6492 signature".to_string();
    let word = |at: usize| body.get(at..at + 32).ok_or_else(bad);
    let small = |w: &[u8]| -> Result<usize, String> {
        if w[..24].iter().any(|b| *b != 0) { return Err(bad()); }
        Ok(u64::from_be_bytes(w[24..32].try_into().unwrap()) as usize)
    };
    let factory = word(0)?;
    if factory[..12].iter().any(|b| *b != 0) {
        return Err(bad());
    }
    let dynamic = |head: usize| -> Result<usize, String> {
        let off = small(word(head)?)?;
        if off % 32 != 0 || off < 96 { return Err(bad()); }
        let len = small(word(off)?)?;
        let end = off.checked_add(32).and_then(|x| x.checked_add(len)).ok_or_else(bad)?;
        if end > body.len() { return Err(bad()); }
        Ok(len)
    };
    let (calldata_len, inner_len) = (dynamic(32)?, dynamic(64)?);
    if inner_len == 0 {
        return Err(bad());
    }
    Ok(Some((format!("0x{}", hex(&factory[12..])), calldata_len, inner_len)))
}

/// The deployless validator call for an ERC-6492 signature: the validator's
/// creation code followed by its constructor arguments.
fn calldata_6492(address: &str, hash: &[u8; 32], sig: &[u8]) -> String {
    let word = |n: usize| format!("{n:064x}");
    let mut padded = hex(sig);
    while padded.len() % 64 != 0 {
        padded.push('0');
    }
    format!("{}{:0>64}{}{}{}{}", ERC6492_VALIDATOR, address.trim_start_matches("0x"), hex(hash), word(96), word(sig.len()), padded)
}

/// How a reader's eth_call answer is read as a vote.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Verdict {
    /// isValidSignature's return: the magic value, left-aligned in a word
    Eip1271,
    /// the ERC-6492 validator's single byte
    Erc6492,
}

fn calldata_1271(hash: &[u8; 32], sig: &[u8]) -> String {
    let word = |n: usize| format!("{n:064x}");
    let mut padded = hex(sig);
    while padded.len() % 64 != 0 {
        padded.push('0');
    }
    format!("0x{}{}{}{}{}", hex(&EIP1271_MAGIC), hex(hash), word(64), word(sig.len()), padded)
}

fn hex_u64(v: &serde_json::Value) -> Option<u64> {
    let s = v.as_str()?.strip_prefix("0x")?;
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

/// 0x-prefixed hex to bytes. Anything that is not ASCII hex digits is refused
/// BEFORE any slicing: a reader's answer is hostile input, and slicing a
/// multi-byte character in half would panic the app.
fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x")?;
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let b = s.as_bytes();
    let nib = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    (0..b.len() / 2).map(|i| Some(nib(b[2 * i])? << 4 | nib(b[2 * i + 1])?)).collect()
}

fn rpc_call(readers: &mut dyn ChainReaders, c: &ContractCfg, url: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let bytes = readers.post(url, body.to_string().as_bytes(), c.timeout_secs, c.max_bytes)?;
    if bytes.len() > c.max_bytes {
        return Err("reader answered too much".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "reader did not answer JSON".to_string())
}

/// A 32-byte hash as 0x + 64 lowercase hex, or None.
fn block_hash(v: Option<&serde_json::Value>) -> Option<String> {
    let h = v?.as_str()?.to_ascii_lowercase();
    (h.len() == 66 && h.starts_with("0x") && h[2..].bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// One reader's COMPLETE answer: the block it read (number, hash, time) and
/// what isValidSignature said there. Two readers agree only when all four
/// match; a verdict is never counted apart from the block it was read at.
#[derive(Clone, PartialEq, Debug)]
struct Snapshot {
    number: u64,
    hash: String,
    timestamp: u64,
    valid: bool,
}

fn first_result(r: &serde_json::Value, id: u64) -> Option<&serde_json::Value> {
    let item = match r {
        serde_json::Value::Array(items) => items.iter().find(|i| i.get("id").and_then(|x| x.as_u64()) == Some(id))?,
        one => one,
    };
    item.get("result")
}

/// What one reader says at block `target`: the block's header first, then the
/// call AT THAT EXACT HASH (EIP-1898 `{blockHash}`), so the verdict is bound to
/// the block the reader named and to nothing else. None when the reader gives
/// no usable answer (down, no such block, a malformed hash, garbage): that is
/// absence, never a vote either way. A revert IS a vote: "not valid".
fn snapshot_at(readers: &mut dyn ChainReaders, c: &ContractCfg, url: &str, target: u64, call_obj: &serde_json::Value, verdict: Verdict) -> Option<Snapshot> {
    let q = serde_json::json!([{ "jsonrpc": "2.0", "id": 1, "method": "eth_getBlockByNumber", "params": [format!("0x{target:x}"), false] }]);
    let r = rpc_call(readers, c, url, &q).ok()?;
    let block = first_result(&r, 1)?;
    if hex_u64(block.get("number")?)? != target {
        return None;
    }
    let hash = block_hash(block.get("hash"))?;
    let timestamp = hex_u64(block.get("timestamp")?)?;
    let call = serde_json::json!([{ "jsonrpc": "2.0", "id": 2, "method": "eth_call",
        "params": [call_obj, { "blockHash": hash, "requireCanonical": true }] }]);
    let r = rpc_call(readers, c, url, &call).ok()?;
    let item = match &r {
        serde_json::Value::Array(items) => items.iter().find(|i| i.get("id").and_then(|x| x.as_u64()) == Some(2))?.clone(),
        one => one.clone(),
    };
    let valid = if let Some(res) = item.get("result") {
        let ret = hex_bytes(res.as_str()?)?; // garbage: no vote
        match verdict {
            Verdict::Eip1271 => ret.len() >= 32 && ret[..4] == EIP1271_MAGIC && ret[4..32].iter().all(|b| *b == 0),
            // exactly one byte, 0 or 1; anything else is no answer at all
            Verdict::Erc6492 => match ret.as_slice() { [1] => true, [0] => false, _ => return None },
        }
    } else {
        // an execution revert is the contract saying no; any other error
        // (unknown block, rate limit, internal) is no answer at all
        let e = item.get("error")?;
        let code = e.get("code").and_then(|x| x.as_i64());
        let msg = e.get("message").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase();
        if code == Some(3) || msg.contains("revert") { false } else { return None }
    };
    Some(Snapshot { number: target, hash, timestamp, valid })
}

/// EIP-1271 by the pinned readers, decided by AGREEMENT ON THE WHOLE ANSWER.
///
///   1. Each reader states its chain id and head. A reader on another chain, or
///      that cannot say, takes no further part.
///   2. The target block is the `quorum`-th highest head (so at least `quorum`
///      readers claim to have it) minus `confirmations`. One reader lagging,
///      stale or lying about its head can neither drag the check to an old
///      block nor push it past what the others have.
///   3. Each remaining reader returns a SNAPSHOT: that block's number, hash and
///      time, and the wallet's verdict read AT THAT HASH: isValidSignature on
///      the wallet for an ordinary contract signature, or - for an EIP-6492
///      signature from a wallet that may not exist yet - the deployless
///      ERC-6492 validator, which simulates the wallet's factory call and asks
///      the same question in one eth_call.
///   4. Identical snapshots are counted. Approve only when exactly one
///      snapshot - and it says valid - is held by at least `quorum` distinct
///      readers, and no other snapshot also reaches `quorum` (a split refuses).
///      Refuse when a "not valid" snapshot reaches quorum, or when nothing
///      does. One reader can therefore neither approve (it is one vote short)
///      nor veto (a lone "no", revert or garbage is outvoted).
///   5. The winning block must be fresh by the clock read AFTER the network
///      reads, not the clock from before them.
pub fn check_1271(c: &ContractCfg, chain_id: u64, address: &str, hash: &[u8; 32], sig: &[u8], readers: RpcPost) -> Result<(), String> {
    // a malformed EIP-6492 wrapper is refused before any reader is asked
    erc6492_parts(sig)?;
    // 1. who is on this chain, and how far each has got
    let mut heads: Vec<(&str, u64)> = Vec::new();
    for url in &c.rpcs {
        let q = serde_json::json!([
            { "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": [] },
            { "jsonrpc": "2.0", "id": 2, "method": "eth_getBlockByNumber", "params": ["latest", false] },
        ]);
        let Ok(r) = rpc_call(&mut *readers, c, url, &q) else { continue };
        let on_chain = first_result(&r, 1).and_then(hex_u64) == Some(chain_id);
        if let (true, Some(n)) = (on_chain, first_result(&r, 2).and_then(|b| b.get("number")).and_then(hex_u64)) {
            heads.push((url.as_str(), n));
        }
    }
    if heads.len() < c.quorum {
        return Err("contract wallet: too few chain readers on this chain answered; try again".into());
    }
    // 2. the block at least `quorum` readers have reached, a few confirmations deep
    let mut ns: Vec<u64> = heads.iter().map(|(_, n)| *n).collect();
    ns.sort_unstable_by(|a, b| b.cmp(a));
    let target = ns[c.quorum - 1].saturating_sub(c.confirmations);
    // 3. complete snapshots, asked the right question for the signature's kind
    let (call_obj, verdict) = match erc6492_parts(sig)? {
        None => (serde_json::json!({ "to": address, "data": calldata_1271(hash, sig) }), Verdict::Eip1271),
        Some(_) => (serde_json::json!({ "data": calldata_6492(address, hash, sig) }), Verdict::Erc6492),
    };
    let snaps: Vec<Snapshot> = heads.iter().filter_map(|(url, _)| snapshot_at(&mut *readers, c, url, target, &call_obj, verdict)).collect();
    // 4. agreement on the whole answer
    let mut tally: Vec<(&Snapshot, usize)> = Vec::new();
    for s in &snaps {
        match tally.iter_mut().find(|(t, _)| *t == s) {
            Some((_, n)) => *n += 1,
            None => tally.push((s, 1)),
        }
    }
    let carried: Vec<&Snapshot> = tally.iter().filter(|(_, n)| *n >= c.quorum).map(|(s, _)| *s).collect();
    let winner = match carried.as_slice() {
        [] => return Err("contract wallet: the chain readers did not agree on an answer; refused (try again)".into()),
        [one] => *one,
        _ => return Err("contract wallet: the chain readers split between answers; refused".into()),
    };
    if !winner.valid {
        return Err("contract wallet: the wallet contract does not accept this signature".into());
    }
    // 5. fresh at decision time
    let now = readers.now();
    if winner.timestamp > now + 60 || now.saturating_sub(winner.timestamp) > c.max_age_secs {
        return Err("contract wallet: the agreed block is stale; refused".into());
    }
    Ok(())
}
