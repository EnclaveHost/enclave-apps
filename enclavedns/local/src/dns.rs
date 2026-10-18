//! DNS for `.eth` (and `.ens`), on this daemon's loopback address. The local
//! DNS proxy forwards those names here; everything else never arrives.
//!
//! Every question goes to EnclaveDNS unchanged. One change on the way back:
//! an `A` question about a name that exists but holds no address records
//! (content, a `url`, only a wallet) is answered with this daemon's address,
//! so a browser comes here and gets the site or a page saying what is
//! missing. Names with records stored on-chain keep them, and names that do
//! not exist stay NXDOMAIN.

use crate::State;
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

const SYNTH_TTL: u32 = 60;
/// when EnclaveDNS is unreachable: come here for the page that says so,
/// but ask again soon
const UNREACHABLE_TTL: u32 = 5;

pub async fn serve_udp(sock: UdpSocket, st: Arc<State>) {
    let sock = Arc::new(sock);
    let mut buf = vec![0u8; 4096];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
        let q = buf[..n].to_vec();
        let (sock, st) = (sock.clone(), st.clone());
        tokio::spawn(async move {
            if let Some(r) = answer(&st, &q).await {
                let _ = sock.send_to(&r, from).await;
            }
        });
    }
}

pub async fn serve_tcp(l: TcpListener, st: Arc<State>) {
    loop {
        let Ok((mut c, _)) = l.accept().await else { continue };
        let st = st.clone();
        tokio::spawn(async move {
            loop {
                let Ok(len) = c.read_u16().await else { return };
                let mut q = vec![0u8; len as usize];
                if c.read_exact(&mut q).await.is_err() {
                    return;
                }
                let Some(r) = answer(&st, &q).await else { return };
                let mut out = (r.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&r);
                if c.write_all(&out).await.is_err() {
                    return;
                }
            }
        });
    }
}

fn is_ens(n: &Name) -> bool {
    let s = n.to_lowercase().to_ascii();
    s.ends_with(".eth.") || s.ends_with(".ens.") || s == "eth." || s == "ens."
}

fn reply(q: &Message, rc: ResponseCode) -> Message {
    let mut m = Message::new();
    m.set_id(q.id())
        .set_message_type(MessageType::Response)
        .set_op_code(q.op_code())
        .set_recursion_desired(q.recursion_desired())
        .set_recursion_available(true)
        .set_response_code(rc);
    m.add_queries(q.queries().to_vec());
    m
}

/// The answer to one DNS message, or None to drop it (unparseable).
pub async fn answer(st: &State, wire: &[u8]) -> Option<Vec<u8>> {
    let q = Message::from_vec(wire).ok()?;
    if q.message_type() != MessageType::Query {
        return None;
    }
    let Some(question) = q.queries().first().cloned() else {
        return reply(&q, ResponseCode::FormErr).to_vec().ok();
    };
    if !is_ens(question.name()) {
        return reply(&q, ResponseCode::Refused).to_vec().ok();
    }
    let qtype = question.query_type();
    let mut r = match st.ens.doh(wire).await.ok().and_then(|b| Message::from_vec(&b).ok()) {
        Some(r) => r,
        None if qtype == RecordType::A => {
            // EnclaveDNS is unreachable: send the browser to the page that says so
            let mut m = reply(&q, ResponseCode::NoError);
            m.add_answer(Record::from_rdata(question.name().clone(), UNREACHABLE_TTL, RData::A(A(st.cfg.gateway_ip))));
            return m.to_vec().ok();
        }
        None => return reply(&q, ResponseCode::ServFail).to_vec().ok(),
    };
    if qtype == RecordType::A && r.response_code() == ResponseCode::NoError {
        // follow the CNAME chain inside the answer (`.ens` aliases to `.eth`)
        let mut name = question.name().to_lowercase();
        for _ in 0..8 {
            let next = r.answers().iter().find_map(|rec| match rec.data() {
                Some(RData::CNAME(c)) if rec.name().to_lowercase() == name => Some(c.0.to_lowercase()),
                _ => None,
            });
            match next {
                Some(n) => name = n,
                None => break,
            }
        }
        let has_a = r.answers().iter().any(|rec| rec.record_type() == RecordType::A && rec.name().to_lowercase() == name);
        if !has_a && is_ens(&name) {
            r.add_answer(Record::from_rdata(name, SYNTH_TTL, RData::A(A(st.cfg.gateway_ip))));
            // a synthesized answer is ours, not the chain's
            r.set_authentic_data(false);
        }
    }
    r.set_id(q.id());
    r.to_vec().ok()
}
