//! The outbound-only libp2p engine: dial peers, run multistream-select +
//! Noise XX + yamux, walk Kademlia to the target key, and PUT_VALUE the
//! record on the closest peers. Everything is one non-blocking state
//! machine multiplexed in the app's event loop — wasm32-wasip2 has no
//! threads, so ~N concurrent dials are N sockets serviced round-robin
//! (the httpd.rs discipline), never N tasks.
//!
//! A connection climbs a fixed ladder, one non-blocking step per drive():
//!   Connecting -> MsSecurity (echo of /noise) -> NoiseAwaitB (send C)
//!   -> MsMux (negotiate /yamux over the Noise channel) -> Muxed
//! Once Muxed it opens yamux streams, negotiates /ipfs/kad/1.0.0 on each,
//! and speaks Kademlia. Layer nesting on the wire: Noise transport messages
//! (u16 length prefix) carry yamux frames, which carry length-prefixed
//! multistream tokens then varint-prefixed kad messages.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;

use crate::kad::{self, CandState, Lookup};
use crate::multiformats::{base58btc, parse_bootstrap, varint, varint_read};
use crate::noise::{self, CipherState};
use crate::yamux;

macro_rules! trace {
    ($($arg:tt)*) => {
        if std::env::var("IPNSPUB_TRACE").is_ok() {
            eprintln!("[trace] {}", format!($($arg)*));
        }
    };
}

const MAX_DIALS: usize = 12;
const CONN_DEADLINE: Duration = Duration::from_secs(45);
const CONN_STALL: Duration = Duration::from_secs(20);
const PUBLISH_DEADLINE: Duration = Duration::from_secs(150);
const TARGET_STORES: usize = kad::K;

// ---- streams ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum StreamPhase {
    ProposeKad, // multistream header + /ipfs/kad/1.0.0 sent, awaiting echo
    Active,     // kad request sent, awaiting reply (or, for PUT, flushed)
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum StreamKind {
    Query,
    GetValue,
    Put,
}

struct Stream {
    id: u32,
    kind: StreamKind,
    phase: StreamPhase,
    recv: Vec<u8>,
    ms_done: bool,
    send_window: u32,
    fin_sent: bool,
}

// ---- per-connection phase --------------------------------------------------

enum Phase {
    Connecting,
    MsSecurity,
    NoiseAwaitB { hs: noise::Handshake },
    MsMux {
        send: CipherState,
        recv: CipherState,
        remote_mh: Vec<u8>,
        plain: Vec<u8>,
        proposed: bool,
    },
    Muxed(Muxed),
    Dead,
}

struct Muxed {
    send: CipherState,
    recv: CipherState,
    remote_mh: Vec<u8>,
    next_stream_id: u32,
    streams: Vec<Stream>,
    plain_in: Vec<u8>,
    out: VecDeque<u8>,
    opened_streams: bool,
}

struct Conn {
    sock: TcpStream,
    peer_mh: Vec<u8>,
    addr: (String, u16),
    phase: Phase,
    rbuf: Vec<u8>,
    wbuf: VecDeque<u8>,
    opened: Instant,
    last_progress: Instant,
    want_query: bool,
    want_put: bool,
    want_get: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Idle,
    Walking,
    Storing,
    Done,
    Failed,
}

pub struct Dht {
    bootstrap_specs: Vec<String>,
    identity: SigningKey,
    lookup: Option<Lookup>,
    routing_key: Vec<u8>,
    record: Vec<u8>,
    conns: Vec<Conn>,
    state: State,
    started: Option<Instant>,
    stores_ok: usize,
    stores_failed: usize,
    get_result: Option<(u64, bool)>,
    last_note: String,
    dialed: Vec<Vec<u8>>,
}

impl Dht {
    pub fn new(bootstrap: Vec<String>, identity: SigningKey) -> Dht {
        Dht {
            bootstrap_specs: bootstrap,
            identity,
            lookup: None,
            routing_key: Vec::new(),
            record: Vec::new(),
            conns: Vec::new(),
            state: State::Idle,
            started: None,
            stores_ok: 0,
            stores_failed: 0,
            get_result: None,
            last_note: "idle".into(),
            dialed: Vec::new(),
        }
    }

    pub fn publish(&mut self, routing_key: Vec<u8>, record: Vec<u8>) {
        if matches!(self.state, State::Walking | State::Storing) {
            self.last_note = "publish already in flight; re-runs on the next timer".into();
            return;
        }
        self.routing_key = routing_key;
        self.record = record;
        self.begin_walk();
    }

    fn begin_walk(&mut self) {
        let mut lookup = Lookup::new(&self.routing_key);
        let mut seeded = 0;
        for spec in &self.bootstrap_specs {
            match parse_bootstrap(spec) {
                Some((host, port, mh)) => {
                    lookup.add_peer(&mh, &[(host, port)]);
                    seeded += 1;
                }
                None => eprintln!("[ipns-publisher] bootstrap not TCP-dialable, skipped: {spec}"),
            }
        }
        if seeded == 0 {
            self.state = State::Failed;
            self.last_note = "no TCP-dialable bootstrap peers".into();
            return;
        }
        self.lookup = Some(lookup);
        self.conns.clear();
        self.dialed.clear();
        self.stores_ok = 0;
        self.stores_failed = 0;
        self.get_result = None;
        self.state = State::Walking;
        self.started = Some(Instant::now());
        self.last_note = format!("walking from {seeded} bootstrap peers");
        eprintln!("[ipns-publisher] DHT: {}", self.last_note);
    }

    /// One event-loop slice. Returns whether work happened.
    pub fn drive(&mut self) -> bool {
        if !matches!(self.state, State::Walking | State::Storing) {
            return false;
        }
        if self.started.map(|t| t.elapsed() > PUBLISH_DEADLINE).unwrap_or(false) {
            self.finish_publish("deadline");
            return true;
        }
        let mut busy = self.spawn_dials();
        let mut i = 0;
        while i < self.conns.len() {
            busy |= self.service_conn(i);
            if matches!(self.conns[i].phase, Phase::Dead) {
                self.reap(i);
            } else {
                i += 1;
            }
        }
        self.check_convergence();
        busy
    }

    fn spawn_dials(&mut self) -> bool {
        if self.state != State::Walking {
            return false;
        }
        let mut busy = false;
        while self.conns.len() < MAX_DIALS {
            let (mh, addrs) = {
                let Some(lookup) = &self.lookup else { break };
                let Some(cand) = lookup.next_fresh() else { break };
                (cand.mh.clone(), cand.addrs.clone())
            };
            if self.dialed.contains(&mh) {
                self.lookup.as_mut().unwrap().mark(&mh, CandState::Querying);
                continue;
            }
            let Some(addr) = addrs.into_iter().next() else {
                self.lookup.as_mut().unwrap().mark(&mh, CandState::Failed);
                continue;
            };
            self.lookup.as_mut().unwrap().mark(&mh, CandState::Querying);
            self.dialed.push(mh.clone());
            match dial_nonblocking(&addr.0, addr.1) {
                Ok(sock) => {
                    let now = Instant::now();
                    self.conns.push(Conn {
                        sock,
                        peer_mh: mh,
                        addr,
                        phase: Phase::Connecting,
                        rbuf: Vec::new(),
                        wbuf: VecDeque::new(),
                        opened: now,
                        last_progress: now,
                        want_query: true,
                        want_put: false,
                        want_get: false,
                    });
                    busy = true;
                }
                Err(e) => {
                    eprintln!("[ipns-publisher] dial {}:{} failed: {e}", addr.0, addr.1);
                    self.lookup.as_mut().unwrap().mark(&mh, CandState::Failed);
                }
            }
        }
        busy
    }

    fn reap(&mut self, i: usize) {
        let conn = self.conns.remove(i);
        trace!("reap {} ({}:{}) last_progress {:?} ago", short(&conn.peer_mh), conn.addr.0, conn.addr.1, conn.last_progress.elapsed());
        if let Some(lookup) = &mut self.lookup {
            if lookup.state_of(&conn.peer_mh) == Some(CandState::Querying) {
                lookup.mark(&conn.peer_mh, CandState::Failed);
            }
        }
    }

    fn check_convergence(&mut self) {
        if self.state == State::Walking {
            let done = self.lookup.as_ref().map(|l| l.done()).unwrap_or(false);
            let stalled = self.conns.is_empty()
                && self.lookup.as_ref().map(|l| l.next_fresh().is_none()).unwrap_or(true);
            if done || stalled {
                self.begin_storing();
            }
        }
        if self.state == State::Storing && self.conns.is_empty() {
            self.finish_publish("store phase complete");
        }
    }

    fn begin_storing(&mut self) {
        let closest = match &self.lookup {
            Some(l) => l.closest(TARGET_STORES),
            None => {
                self.state = State::Failed;
                return;
            }
        };
        if closest.is_empty() {
            self.finish_publish("no peers responded to the walk");
            return;
        }
        eprintln!("[ipns-publisher] DHT: walk converged, storing on {} peers", closest.len());
        self.last_note = format!("storing on {} peers", closest.len());
        self.state = State::Storing;
        self.conns.clear();
        self.dialed.clear();
        for (n, mh) in closest.iter().enumerate() {
            let addrs = self.lookup.as_ref().unwrap().addrs_of(mh);
            let Some(addr) = addrs.into_iter().next() else {
                self.stores_failed += 1;
                continue;
            };
            match dial_nonblocking(&addr.0, addr.1) {
                Ok(sock) => {
                    let now = Instant::now();
                    self.conns.push(Conn {
                        sock,
                        peer_mh: mh.clone(),
                        addr,
                        phase: Phase::Connecting,
                        rbuf: Vec::new(),
                        wbuf: VecDeque::new(),
                        opened: now,
                        last_progress: now,
                        want_query: false,
                        want_put: true,
                        want_get: n == 0, // read back from the closest peer
                    });
                }
                Err(_) => self.stores_failed += 1,
            }
        }
        if self.conns.is_empty() {
            self.finish_publish("no store peers dialable");
        }
    }

    fn finish_publish(&mut self, why: &str) {
        self.conns.clear();
        self.state = if self.stores_ok > 0 { State::Done } else { State::Failed };
        self.last_note = format!(
            "{why}: {} stored, {} failed{}",
            self.stores_ok,
            self.stores_failed,
            match self.get_result {
                Some((seq, true)) => format!(", read-back verified seq {seq}"),
                Some((seq, false)) => format!(", read-back seq {seq} FAILED verify"),
                None => String::new(),
            }
        );
        eprintln!("[ipns-publisher] DHT: {}", self.last_note);
    }

    // ---- per-connection stepping -------------------------------------------

    fn service_conn(&mut self, i: usize) -> bool {
        {
            let c = &self.conns[i];
            if c.opened.elapsed() > CONN_DEADLINE || c.last_progress.elapsed() > CONN_STALL {
                trace!("{} timed out in {} (rbuf {})", short(&c.peer_mh), phase_name(&c.phase), c.rbuf.len());
                self.conns[i].phase = Phase::Dead;
                return true;
            }
        }
        let mut busy = false;
        busy |= self.flush_socket(i);
        busy |= self.read_socket(i);
        busy |= self.advance_phase(i);
        busy |= self.flush_socket(i);
        busy
    }

    fn flush_socket(&mut self, i: usize) -> bool {
        let c = &mut self.conns[i];
        let mut moved = false;
        while !c.wbuf.is_empty() {
            let (front, _) = c.wbuf.as_slices();
            match c.sock.write(front) {
                Ok(0) => {
                    trace!("{} flush wrote 0 (wbuf {}, phase {})", short(&c.peer_mh), c.wbuf.len(), phase_name(&c.phase));
                    c.phase = Phase::Dead;
                    break;
                }
                Ok(n) => {
                    c.wbuf.drain(..n);
                    c.last_progress = Instant::now();
                    moved = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    trace!("{} write err: {e} (kind {:?})", short(&c.peer_mh), e.kind());
                    c.phase = Phase::Dead;
                    break;
                }
            }
        }
        moved
    }

    fn read_socket(&mut self, i: usize) -> bool {
        let c = &mut self.conns[i];
        let mut buf = [0u8; 32 * 1024];
        let mut moved = false;
        loop {
            match c.sock.read(&mut buf) {
                Ok(0) => {
                    if !matches!(c.phase, Phase::Connecting) {
                        trace!("{} read EOF (had {} rbuf)", short(&c.peer_mh), c.rbuf.len());
                        c.phase = Phase::Dead;
                    }
                    break;
                }
                Ok(n) => {
                    c.rbuf.extend_from_slice(&buf[..n]);
                    c.last_progress = Instant::now();
                    moved = true;
                    if c.rbuf.len() > 4 * 1024 * 1024 {
                        c.phase = Phase::Dead;
                        break;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::TimedOut => break,
                Err(e) => {
                    trace!("{} read err: {e} (kind {:?})", short(&c.peer_mh), e.kind());
                    c.phase = Phase::Dead;
                    break;
                }
            }
        }
        moved
    }

    fn advance_phase(&mut self, i: usize) -> bool {
        // Take the phase out to work on it (steps mutate other conn fields
        // freely while it is owned here); the temporary is Dead only so the
        // slot holds something. Steps signal death by RETURNING Phase::Dead,
        // and a read/flush that already set Dead is pulled out here and
        // re-matched to the Dead arm — either way `new_phase` is authoritative.
        let phase = std::mem::replace(&mut self.conns[i].phase, Phase::Dead);
        let (new_phase, busy) = match phase {
            Phase::Connecting => self.step_connecting(i),
            Phase::MsSecurity => self.step_ms_security(i),
            Phase::NoiseAwaitB { hs } => self.step_noise(i, hs),
            Phase::MsMux { send, recv, remote_mh, plain, proposed } => {
                self.step_ms_mux(i, send, recv, remote_mh, plain, proposed)
            }
            Phase::Muxed(muxed) => self.step_muxed(i, muxed),
            Phase::Dead => (Phase::Dead, false),
        };
        self.conns[i].phase = new_phase;
        busy
    }

    fn step_connecting(&mut self, i: usize) -> (Phase, bool) {
        // Non-blocking connect completes when the socket accepts a write. We
        // send the security multistream proposal optimistically as that probe.
        let mut first = ms_message(MS_HEADER);
        first.extend_from_slice(&ms_message(b"/noise\n"));
        let c = &mut self.conns[i];
        match c.sock.write(&first) {
            Ok(n) => {
                c.last_progress = Instant::now();
                if n < first.len() {
                    c.wbuf.extend(&first[n..]);
                }
                trace!("{} connected, sent security proposal", short(&c.peer_mh));
                (Phase::MsSecurity, true)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => (Phase::Connecting, false),
            Err(_) => (Phase::Dead, true),
        }
    }

    fn step_ms_security(&mut self, i: usize) -> (Phase, bool) {
        let c = &mut self.conns[i];
        match drain_ms_tokens(&mut c.rbuf, 2) {
            Ok(Some(tokens)) => {
                let ok = tokens.iter().any(|t| t == MS_HEADER)
                    && tokens.iter().any(|t| t == b"/noise\n");
                if !ok {
                    eprintln!("[ipns-publisher] {}: security mismatch", short(&c.peer_mh));
                    return (Phase::Dead, true);
                }
                // start Noise: send message A immediately
                trace!("{} security ok, sending noise A", short(&c.peer_mh));
                let (hs, msg_a) = noise::Handshake::start(self.identity.clone());
                self.conns[i].wbuf.extend(len_prefixed_u16(&msg_a));
                self.conns[i].last_progress = Instant::now();
                (Phase::NoiseAwaitB { hs }, true)
            }
            Ok(None) => {
                trace!("{} ms-security waiting (rbuf {}: {})", short(&c.peer_mh), c.rbuf.len(), crate::multiformats::hex(&c.rbuf[..c.rbuf.len().min(32)]));
                (Phase::MsSecurity, false)
            }
            Err(_) => {
                trace!("{} ms-security drain ERR (rbuf {}: {})", short(&c.peer_mh), c.rbuf.len(), crate::multiformats::hex(&c.rbuf[..c.rbuf.len().min(32)]));
                (Phase::Dead, true)
            }
        }
    }

    fn step_noise(&mut self, i: usize, hs: noise::Handshake) -> (Phase, bool) {
        let msg_b = match take_u16_frame(&mut self.conns[i].rbuf) {
            Some(m) => m,
            None => return (Phase::NoiseAwaitB { hs }, false),
        };
        match hs.read_b(&msg_b) {
            Ok((msg_c, est)) => {
                let c = &mut self.conns[i];
                if !c.peer_mh.is_empty() && est.remote.peer_mh != c.peer_mh {
                    eprintln!(
                        "[ipns-publisher] {}: peer id mismatch (reached {})",
                        short(&c.peer_mh),
                        short(&est.remote.peer_mh)
                    );
                    return (Phase::Dead, true);
                }
                c.wbuf.extend(len_prefixed_u16(&msg_c));
                c.last_progress = Instant::now();
                trace!("{} noise complete, remote {}", short(&c.peer_mh), short(&est.remote.peer_mh));
                (
                    Phase::MsMux {
                        send: est.send,
                        recv: est.recv,
                        remote_mh: est.remote.peer_mh,
                        plain: Vec::new(),
                        proposed: false,
                    },
                    true,
                )
            }
            Err(e) => {
                eprintln!("[ipns-publisher] {}: noise failed: {e}", short(&self.conns[i].peer_mh));
                (Phase::Dead, true)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_ms_mux(
        &mut self,
        i: usize,
        mut send: CipherState,
        mut recv: CipherState,
        remote_mh: Vec<u8>,
        mut plain: Vec<u8>,
        mut proposed: bool,
    ) -> (Phase, bool) {
        let mut busy = false;
        // decrypt available transport messages into `plain`
        while let Some(ct) = take_u16_frame(&mut self.conns[i].rbuf) {
            match recv.open(&[], &ct) {
                Ok(pt) => {
                    plain.extend_from_slice(&pt);
                    busy = true;
                }
                Err(e) => {
                    eprintln!("[ipns-publisher] {}: mux decrypt failed: {e}", short(&remote_mh));
                    return (Phase::Dead, true);
                }
            }
        }
        if !proposed {
            let mut msg = ms_message(MS_HEADER);
            msg.extend_from_slice(&ms_message(b"/yamux/1.0.0\n"));
            let ct = send.seal(&[], &msg);
            self.conns[i].wbuf.extend(len_prefixed_u16(&ct));
            self.conns[i].last_progress = Instant::now();
            proposed = true;
            busy = true;
        }
        match drain_ms_tokens(&mut plain, 2) {
            Ok(Some(tokens)) => {
                let ok = tokens.iter().any(|t| t == MS_HEADER)
                    && tokens.iter().any(|t| t == b"/yamux/1.0.0\n");
                if !ok {
                    eprintln!("[ipns-publisher] {}: yamux mismatch", short(&remote_mh));
                    return (Phase::Dead, true);
                }
                let mut muxed = Muxed {
                    send,
                    recv,
                    remote_mh,
                    next_stream_id: 1,
                    streams: Vec::new(),
                    plain_in: plain, // leftover belongs to yamux
                    out: VecDeque::new(),
                    opened_streams: false,
                };
                trace!("{} yamux negotiated, opening kad streams", short(&muxed.remote_mh));
                self.open_initial_streams(i, &mut muxed);
                (Phase::Muxed(muxed), true)
            }
            Ok(None) => (Phase::MsMux { send, recv, remote_mh, plain, proposed }, busy),
            Err(_) => (Phase::Dead, true),
        }
    }

    fn open_initial_streams(&mut self, i: usize, muxed: &mut Muxed) {
        let c = &self.conns[i];
        let mut kinds = Vec::new();
        if c.want_query {
            kinds.push(StreamKind::Query);
        }
        if c.want_get {
            kinds.push(StreamKind::GetValue);
        }
        if c.want_put {
            kinds.push(StreamKind::Put);
        }
        for kind in kinds {
            let id = muxed.next_stream_id;
            muxed.next_stream_id += 2;
            let mut plain = ms_message(MS_HEADER);
            plain.extend_from_slice(&ms_message(format!("{}\n", kad::PROTO).as_bytes()));
            let frame = yamux::data(id, yamux::FLAG_SYN, plain.len() as u32);
            let mut wire = frame.encode().to_vec();
            wire.extend_from_slice(&plain);
            queue_muxed(muxed, &wire);
            muxed.streams.push(Stream {
                id,
                kind,
                phase: StreamPhase::ProposeKad,
                recv: Vec::new(),
                ms_done: false,
                send_window: yamux::INITIAL_WINDOW,
                fin_sent: false,
            });
        }
        muxed.opened_streams = true;
    }

    fn step_muxed(&mut self, i: usize, mut muxed: Muxed) -> (Phase, bool) {
        let mut busy = false;
        // decrypt inbound transport messages
        while let Some(ct) = take_u16_frame(&mut self.conns[i].rbuf) {
            match muxed.recv.open(&[], &ct) {
                Ok(pt) => {
                    muxed.plain_in.extend_from_slice(&pt);
                    busy = true;
                }
                Err(_) => return (Phase::Dead, true),
            }
        }
        // parse yamux frames
        match self.pump_yamux(&mut muxed) {
            Ok(b) => busy |= b,
            Err(()) => return (Phase::Dead, true),
        }
        // advance kad protocol per stream
        busy |= self.pump_streams(&mut muxed);
        // flush queued encrypted output
        if !muxed.out.is_empty() {
            let bytes: Vec<u8> = muxed.out.drain(..).collect();
            self.conns[i].wbuf.extend(bytes);
            self.conns[i].last_progress = Instant::now();
            busy = true;
        }
        if muxed.opened_streams
            && !muxed.streams.is_empty()
            && muxed.streams.iter().all(|s| s.phase == StreamPhase::Done)
        {
            return (Phase::Dead, true);
        }
        (Phase::Muxed(muxed), busy)
    }

    fn pump_yamux(&mut self, muxed: &mut Muxed) -> Result<bool, ()> {
        let mut busy = false;
        loop {
            if muxed.plain_in.len() < yamux::HEADER {
                break;
            }
            let frame = yamux::Frame::decode(&muxed.plain_in[..yamux::HEADER]).map_err(|_| ())?;
            match frame.typ {
                yamux::TYPE_DATA => {
                    let total = yamux::HEADER + frame.length as usize;
                    if muxed.plain_in.len() < total {
                        break;
                    }
                    let body = muxed.plain_in[yamux::HEADER..total].to_vec();
                    muxed.plain_in.drain(..total);
                    busy = true;
                    if let Some(s) = muxed.streams.iter_mut().find(|s| s.id == frame.stream_id) {
                        s.recv.extend_from_slice(&body);
                    }
                    if !body.is_empty() {
                        let wu = yamux::window_update(frame.stream_id, 0, body.len() as u32);
                        queue_muxed(muxed, &wu.encode());
                    }
                }
                yamux::TYPE_WINDOW => {
                    if let Some(s) = muxed.streams.iter_mut().find(|s| s.id == frame.stream_id) {
                        s.send_window = s.send_window.saturating_add(frame.length);
                    }
                    muxed.plain_in.drain(..yamux::HEADER);
                    busy = true;
                }
                yamux::TYPE_PING => {
                    if frame.flags & yamux::FLAG_SYN != 0 {
                        let pong = yamux::ping(yamux::FLAG_ACK, frame.length);
                        queue_muxed(muxed, &pong.encode());
                    }
                    muxed.plain_in.drain(..yamux::HEADER);
                    busy = true;
                }
                yamux::TYPE_GOAWAY => {
                    muxed.plain_in.drain(..yamux::HEADER);
                    return Err(());
                }
                _ => {
                    muxed.plain_in.drain(..yamux::HEADER);
                }
            }
        }
        Ok(busy)
    }

    fn pump_streams(&mut self, muxed: &mut Muxed) -> bool {
        let mut busy = false;
        let mut outbox: Vec<Vec<u8>> = Vec::new();
        let mut walk_updates: Vec<(Vec<u8>, Vec<(String, u16)>)> = Vec::new();
        let mut responded: Vec<Vec<u8>> = Vec::new();
        let mut store_ok = 0usize;
        let mut get_seq: Option<(u64, bool)> = None;
        let peer_mh = muxed.remote_mh.clone();

        for s in muxed.streams.iter_mut() {
            if s.phase == StreamPhase::Done {
                continue;
            }
            if !s.ms_done {
                match drain_ms_tokens(&mut s.recv, 2) {
                    Ok(Some(tokens)) => {
                        let ok = tokens.iter().any(|t| t == MS_HEADER)
                            && tokens.iter().any(|t| t == format!("{}\n", kad::PROTO).as_bytes());
                        if !ok {
                            s.phase = StreamPhase::Done;
                            continue;
                        }
                        s.ms_done = true;
                        busy = true;
                        let payload = match s.kind {
                            StreamKind::Query => kad::find_node(&self.routing_key),
                            StreamKind::GetValue => kad::get_value(&self.routing_key),
                            StreamKind::Put => kad::put_value(&self.routing_key, &self.record),
                        };
                        outbox.push(stream_data(s.id, &len_prefixed_varint(&payload)));
                        if matches!(s.kind, StreamKind::Put) {
                            outbox.push(fin_frame(s.id));
                            s.fin_sent = true;
                        }
                        s.phase = StreamPhase::Active;
                    }
                    Ok(None) => continue,
                    Err(_) => {
                        s.phase = StreamPhase::Done;
                        continue;
                    }
                }
            }
            if s.phase != StreamPhase::Active {
                continue;
            }
            match s.kind {
                StreamKind::Put => {
                    // A PUT_VALUE gets no reply; once our request + FIN are
                    // queued the peer has it. A peer that rejects resets the
                    // stream / GOAWAYs, which reaps the conn before this.
                    store_ok += 1;
                    s.phase = StreamPhase::Done;
                    busy = true;
                }
                StreamKind::Query | StreamKind::GetValue => {
                    if let Some(msg) = take_varint_frame(&mut s.recv) {
                        busy = true;
                        if let Some(km) = kad::parse_message(&msg) {
                            for p in &km.closer {
                                walk_updates.push((p.mh.clone(), p.tcp_addrs.clone()));
                            }
                            responded.push(peer_mh.clone());
                            if matches!(s.kind, StreamKind::GetValue) {
                                if let Some((_k, val)) = &km.record {
                                    get_seq = Some(verify_get(val, &self.routing_key));
                                }
                            }
                        }
                        s.phase = StreamPhase::Done;
                    }
                }
            }
        }
        for w in outbox {
            queue_muxed(muxed, &w);
        }
        if let Some(lookup) = &mut self.lookup {
            for (mh, addrs) in walk_updates {
                lookup.add_peer(&mh, &addrs);
            }
            for mh in &responded {
                lookup.mark(mh, CandState::Responded);
            }
        }
        self.stores_ok += store_ok;
        if let Some(g) = get_seq {
            self.get_result = Some(g);
        }
        busy
    }

    // ---- status ------------------------------------------------------------

    pub fn status_json(&self) -> String {
        format!(
            "{{\"state\":\"{}\",\"conns\":{},\"storesOk\":{},\"storesFailed\":{},\"note\":\"{}\"}}",
            state_str(self.state),
            self.conns.len(),
            self.stores_ok,
            self.stores_failed,
            crate::httpd::json_escape(&self.last_note),
        )
    }

    pub fn status_line(&self) -> String {
        format!("{}: {}", state_str(self.state), self.last_note)
    }
}

fn verify_get(record: &[u8], routing_key: &[u8]) -> (u64, bool) {
    let mh = &routing_key[6..]; // strip "/ipns/"
    match crate::ipns::peer_mh_pubkey(mh) {
        Some(pk) => match crate::ipns::verify_record(record, &pk) {
            Ok(rec) => (rec.sequence, true),
            Err(_) => (0, false),
        },
        None => (0, false),
    }
}

fn state_str(s: State) -> &'static str {
    match s {
        State::Idle => "idle",
        State::Walking => "walking",
        State::Storing => "storing",
        State::Done => "done",
        State::Failed => "failed",
    }
}

// ---- multistream + framing helpers -----------------------------------------

const MS_HEADER: &[u8] = b"/multistream/1.0.0\n";

fn ms_message(token: &[u8]) -> Vec<u8> {
    len_prefixed_varint(token)
}

fn len_prefixed_varint(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 2);
    varint(&mut out, data.len() as u64);
    out.extend_from_slice(data);
    out
}

fn len_prefixed_u16(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 2);
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Pull exactly `want` multistream tokens out of `buf`, but only once all
/// `want` are complete in the buffer — multistream sends the header and the
/// protocol echo in separate segments, so a partial read must not decide
/// negotiation. Returns Ok(None) (buffer untouched) until `want` are present.
fn drain_ms_tokens(buf: &mut Vec<u8>, want: usize) -> Result<Option<Vec<Vec<u8>>>, ()> {
    let mut tokens = Vec::new();
    let mut off = 0usize;
    while tokens.len() < want {
        let Some((len, n)) = varint_read(&buf[off..]) else { return Ok(None) };
        if len > 1024 {
            return Err(());
        }
        if buf.len() < off + n + len as usize {
            return Ok(None); // this token not fully arrived yet
        }
        tokens.push(buf[off + n..off + n + len as usize].to_vec());
        off += n + len as usize;
    }
    buf.drain(..off);
    Ok(Some(tokens))
}

fn take_u16_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + len {
        return None;
    }
    let frame = buf[2..2 + len].to_vec();
    buf.drain(..2 + len);
    Some(frame)
}

fn take_varint_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (len, n) = varint_read(buf)?;
    if buf.len() < n + len as usize {
        return None;
    }
    let frame = buf[n..n + len as usize].to_vec();
    buf.drain(..n + len as usize);
    Some(frame)
}

fn stream_data(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = yamux::data(id, 0, payload.len() as u32).encode().to_vec();
    out.extend_from_slice(payload);
    out
}

fn fin_frame(id: u32) -> Vec<u8> {
    yamux::data(id, yamux::FLAG_FIN, 0).encode().to_vec()
}

/// Seal a plaintext yamux frame and queue it as Noise transport messages.
/// A Noise message caps plaintext at MAX_PLAINTEXT, so large frames split.
fn queue_muxed(muxed: &mut Muxed, plain: &[u8]) {
    for chunk in plain.chunks(noise::MAX_PLAINTEXT) {
        let ct = muxed.send.seal(&[], chunk);
        muxed.out.extend(len_prefixed_u16(&ct));
    }
}

fn short(mh: &[u8]) -> String {
    let s = base58btc(mh);
    if s.len() > 8 {
        format!("…{}", &s[s.len() - 8..])
    } else {
        s
    }
}

fn dial_nonblocking(host: &str, port: u16) -> Result<TcpStream, String> {
    // dial() goes through the fleet egress and returns a blocking-connected
    // stream (the SOCKS CONNECT completes inline). Non-blocking afterwards so
    // the rest of the ladder is event-loop friendly.
    let sock = crate::egress::dial(host, port, Some(Duration::from_secs(15)))?;
    sock.set_nonblocking(true).map_err(|e| format!("set_nonblocking: {e}"))?;
    Ok(sock)
}

fn phase_name(p: &Phase) -> &'static str {
    match p {
        Phase::Connecting => "connecting",
        Phase::MsSecurity => "ms-security",
        Phase::NoiseAwaitB { .. } => "noise",
        Phase::MsMux { .. } => "ms-mux",
        Phase::Muxed(_) => "muxed",
        Phase::Dead => "dead",
    }
}
