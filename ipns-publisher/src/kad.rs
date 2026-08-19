//! Kademlia, the client half only: the /ipfs/kad/1.0.0 message codec and
//! the iterative-lookup bookkeeping. Sans-io — p2p.rs owns sockets and
//! feeds responses in. Spec: libp2p/specs/kad-dht; wire messages are
//! varint-length-prefixed protobufs (the stream layer adds that prefix).
//!
//! Distance is Kademlia XOR over SHA256: a peer's kad ID is SHA256 of its
//! peer-ID multihash bytes, the target is SHA256 of the routing key.

#![allow(dead_code)]

use sha2::{Digest, Sha256};

use crate::ipns::{pb_bytes, pb_scan, pb_uint};
use crate::multiformats::{self, Seg};

pub const PROTO: &str = "/ipfs/kad/1.0.0";
/// Replication parameter: how many closest peers store the record.
pub const K: usize = 20;

pub const PUT_VALUE: u64 = 0;
pub const GET_VALUE: u64 = 1;
pub const FIND_NODE: u64 = 4;

// ---- message codec ---------------------------------------------------------

pub fn find_node(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 8);
    pb_uint(&mut out, 1, FIND_NODE);
    pb_bytes(&mut out, 2, key);
    out
}

pub fn get_value(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len() + 8);
    pb_uint(&mut out, 1, GET_VALUE);
    pb_bytes(&mut out, 2, key);
    out
}

pub fn put_value(key: &[u8], record: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(key.len() + record.len() + 8);
    pb_bytes(&mut rec, 1, key);
    pb_bytes(&mut rec, 2, record);
    let mut out = Vec::with_capacity(rec.len() + key.len() + 12);
    pb_uint(&mut out, 1, PUT_VALUE);
    pb_bytes(&mut out, 2, key);
    pb_bytes(&mut out, 3, &rec);
    out
}

pub struct KadPeer {
    pub mh: Vec<u8>,
    /// Dialable TCP targets only (Step 0: no UDP egress).
    pub tcp_addrs: Vec<(String, u16)>,
}

pub struct KadMessage {
    pub typ: u64,
    pub closer: Vec<KadPeer>,
    /// Record { key, value } when the response carries one.
    pub record: Option<(Vec<u8>, Vec<u8>)>,
}

pub fn parse_message(bytes: &[u8]) -> Option<KadMessage> {
    let mut typ = u64::MAX;
    let mut closer = Vec::new();
    let mut record = None;
    pb_scan(bytes, |field, wire, data| match (field, wire) {
        (1, 0) => typ = u64::from_le_bytes(data.try_into().unwrap_or([0; 8])),
        (3, 2) => {
            let mut key = Vec::new();
            let mut value = Vec::new();
            if pb_scan(data, |f, w, d| match (f, w) {
                (1, 2) => key = d.to_vec(),
                (2, 2) => value = d.to_vec(),
                _ => {}
            })
            .is_some()
                && !value.is_empty()
            {
                record = Some((key, value));
            }
        }
        (8, 2) => {
            let mut mh = Vec::new();
            let mut tcp_addrs = Vec::new();
            if pb_scan(data, |f, w, d| match (f, w) {
                (1, 2) => mh = d.to_vec(),
                (2, 2) => {
                    if let Some(segs) = multiformats::multiaddr_decode(d) {
                        if !segs.iter().any(|s| matches!(s, Seg::Circuit)) {
                            if let Some(t) = multiformats::tcp_target(&segs) {
                                tcp_addrs.push(t);
                            }
                        }
                    }
                }
                _ => {}
            })
            .is_some()
                && !mh.is_empty()
            {
                closer.push(KadPeer { mh, tcp_addrs });
            }
        }
        _ => {}
    })?;
    if typ == u64::MAX {
        return None;
    }
    Some(KadMessage { typ, closer, record })
}

// ---- iterative lookup ------------------------------------------------------

pub fn kad_id(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// XOR-distance comparison of two kad IDs to a target: is `a` closer than `b`?
pub fn closer(a: &[u8; 32], b: &[u8; 32], target: &[u8; 32]) -> bool {
    for i in 0..32 {
        let (da, db) = (a[i] ^ target[i], b[i] ^ target[i]);
        if da != db {
            return da < db;
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CandState {
    Fresh,     // known, not yet queried
    Querying,  // a FIND_NODE/GET_VALUE is in flight (or dialing)
    Responded, // answered a query
    Failed,    // undialable or errored
}

pub struct Candidate {
    pub mh: Vec<u8>,
    pub kad_id: [u8; 32],
    pub addrs: Vec<(String, u16)>,
    pub state: CandState,
}

pub struct Lookup {
    pub target: [u8; 32],
    /// All candidates, kept sorted by distance to the target.
    pub cands: Vec<Candidate>,
}

impl Lookup {
    pub fn new(routing_key: &[u8]) -> Lookup {
        Lookup { target: kad_id(routing_key), cands: Vec::new() }
    }

    /// Merge a peer into the candidate set (new addresses extend, state is
    /// kept). Peers with no dialable address still enter as Failed so the
    /// walk knows they exist and cannot be stored to.
    pub fn add_peer(&mut self, mh: &[u8], addrs: &[(String, u16)]) {
        if let Some(c) = self.cands.iter_mut().find(|c| c.mh == mh) {
            for a in addrs {
                if !c.addrs.contains(a) {
                    c.addrs.push(a.clone());
                }
            }
            if c.state == CandState::Failed && !addrs.is_empty() {
                c.state = CandState::Fresh; // learned a new way in
            }
            return;
        }
        let kad_id = kad_id(mh);
        let state = if addrs.is_empty() { CandState::Failed } else { CandState::Fresh };
        let cand = Candidate { mh: mh.to_vec(), kad_id, addrs: addrs.to_vec(), state };
        let pos = self
            .cands
            .iter()
            .position(|c| closer(&cand.kad_id, &c.kad_id, &self.target))
            .unwrap_or(self.cands.len());
        self.cands.insert(pos, cand);
    }

    pub fn mark(&mut self, mh: &[u8], state: CandState) {
        if let Some(c) = self.cands.iter_mut().find(|c| c.mh == mh) {
            c.state = state;
        }
    }

    pub fn state_of(&self, mh: &[u8]) -> Option<CandState> {
        self.cands.iter().find(|c| c.mh == mh).map(|c| c.state)
    }

    /// The closest Fresh candidate that is worth querying: within the
    /// closest `beyond` candidates that are not Failed, or closer than the
    /// k-th Responded peer. Returns its multihash.
    pub fn next_fresh(&self) -> Option<&Candidate> {
        let kth = self.kth_responded(K);
        self.cands.iter().find(|c| {
            c.state == CandState::Fresh
                && match kth {
                    Some(k) => closer(&c.kad_id, k, &self.target),
                    None => true,
                }
        })
    }

    fn kth_responded(&self, k: usize) -> Option<&[u8; 32]> {
        self.cands
            .iter()
            .filter(|c| c.state == CandState::Responded)
            .nth(k - 1)
            .map(|c| &c.kad_id)
    }

    /// Queries in flight.
    pub fn querying(&self) -> usize {
        self.cands.iter().filter(|c| c.state == CandState::Querying).count()
    }

    pub fn responded(&self) -> usize {
        self.cands.iter().filter(|c| c.state == CandState::Responded).count()
    }

    /// The walk has converged: at least one response, nothing in flight,
    /// and no Fresh candidate closer than the k-th responded peer.
    pub fn done(&self) -> bool {
        self.responded() > 0 && self.querying() == 0 && self.next_fresh().is_none()
    }

    /// The k closest peers that responded (the store set).
    pub fn closest(&self, k: usize) -> Vec<Vec<u8>> {
        self.cands
            .iter()
            .filter(|c| c.state == CandState::Responded)
            .take(k)
            .map(|c| c.mh.clone())
            .collect()
    }

    pub fn addrs_of(&self, mh: &[u8]) -> Vec<(String, u16)> {
        self.cands
            .iter()
            .find(|c| c.mh == mh)
            .map(|c| c.addrs.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip_via_parse() {
        let key = b"/ipns/binaryid";
        let msg = find_node(key);
        let parsed = parse_message(&msg).unwrap();
        assert_eq!(parsed.typ, FIND_NODE);
        assert!(parsed.record.is_none());

        let put = put_value(key, b"recordbytes");
        let parsed = parse_message(&put).unwrap();
        assert_eq!(parsed.typ, PUT_VALUE);
        let (rkey, rval) = parsed.record.unwrap();
        assert_eq!(rkey, key);
        assert_eq!(rval, b"recordbytes");
    }

    #[test]
    fn closer_peers_filtered_to_tcp() {
        // Peer message with one TCP addr and one QUIC addr
        let mut peer = Vec::new();
        pb_bytes(&mut peer, 1, b"\x00\x04peer");
        let tcp = [0x04, 1, 2, 3, 4, 0x06, 0x0f, 0xa1]; // /ip4/1.2.3.4/tcp/4001
        let quic = [0x04, 1, 2, 3, 4, 0x91, 0x02, 0x0f, 0xa1, 0xcd, 0x03];
        pb_bytes(&mut peer, 2, &tcp);
        pb_bytes(&mut peer, 2, &quic);
        let mut msg = Vec::new();
        pb_uint(&mut msg, 1, FIND_NODE);
        pb_bytes(&mut msg, 8, &peer);
        let parsed = parse_message(&msg).unwrap();
        assert_eq!(parsed.closer.len(), 1);
        assert_eq!(parsed.closer[0].tcp_addrs, vec![("1.2.3.4".to_string(), 4001)]);
    }

    #[test]
    fn lookup_convergence() {
        let mut lk = Lookup::new(b"/ipns/target");
        // seed with two peers; the walk queries both, learns a third, ends
        lk.add_peer(b"peerA", &[("10.0.0.1".into(), 4001)]);
        lk.add_peer(b"peerB", &[("10.0.0.2".into(), 4001)]);
        assert!(!lk.done());
        let first = lk.next_fresh().unwrap().mh.clone();
        lk.mark(&first, CandState::Querying);
        let second = lk.next_fresh().unwrap().mh.clone();
        assert_ne!(first, second);
        lk.mark(&second, CandState::Querying);
        assert!(lk.next_fresh().is_none());
        lk.mark(&first, CandState::Responded);
        lk.mark(&second, CandState::Responded);
        lk.add_peer(b"peerC", &[("10.0.0.3".into(), 4001)]);
        assert!(!lk.done()); // C is fresh and within the closest K
        let third = lk.next_fresh().unwrap().mh.clone();
        lk.mark(&third, CandState::Querying);
        lk.mark(&third, CandState::Failed);
        assert!(lk.done());
        assert_eq!(lk.closest(20).len(), 2);
        // candidates stay distance-sorted
        for w in lk.cands.windows(2) {
            assert!(!closer(&w[1].kad_id, &w[0].kad_id, &lk.target));
        }
    }

    #[test]
    fn addr_merge_revives_failed() {
        let mut lk = Lookup::new(b"k");
        lk.add_peer(b"p", &[]);
        assert_eq!(lk.state_of(b"p"), Some(CandState::Failed));
        lk.add_peer(b"p", &[("1.1.1.1".into(), 4001)]);
        assert_eq!(lk.state_of(b"p"), Some(CandState::Fresh));
        assert_eq!(lk.addrs_of(b"p").len(), 1);
    }
}
