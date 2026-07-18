//! irc server state and command handling.
//!
//! Single-threaded by construction: wasm32-wasip2 has no threads, so all
//! connection state lives in one `Server` and the main loop pumps
//! non-blocking sockets. Every handler follows the same borrow discipline:
//! read what you need out of the maps first, then mutate / enqueue output.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::message::{self};
use crate::{CHANNEL_MAX, NICK_MAX, TOPIC_MAX};

pub const SERVER_NAME: &str = "irc.enclave.host";
const NETWORK: &str = "Enclave";
const VERSION_STR: &str = concat!("irc-", env!("CARGO_PKG_VERSION"));

const MAX_CLIENTS: usize = 512;
const CHANS_PER_USER: usize = 32;
const MSG_TARGETS_MAX: usize = 4;
const READ_CHUNK: usize = 4096; // per-client per-tick read cap = crude flood control
const INBUF_MAX: usize = 16 * 1024;
const OUTBUF_MAX: usize = 256 * 1024;
const LINE_MAX: usize = 510; // excluding CRLF, per RFC 2812

const PING_INTERVAL: Duration = Duration::from_secs(90);
const PONG_TIMEOUT: Duration = Duration::from_secs(120);
const REG_TIMEOUT: Duration = Duration::from_secs(60);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Conn {
    stream: TcpStream,
    host: String,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    outpos: usize,
    nick: Option<String>,
    user: Option<String>,
    realname: String,
    registered: bool,
    cap_negotiating: bool,
    away: Option<String>,
    channels: HashSet<String>, // lowercased channel names
    connected_at: Instant,
    last_recv: Instant,
    signon: u64,
    idle_since: u64,
    awaiting_pong: bool,
    ping_sent: Instant,
    quit: Option<String>,
}

impl Conn {
    fn new(stream: TcpStream, addr: SocketAddr) -> Self {
        let now = Instant::now();
        Conn {
            stream,
            host: addr.ip().to_string(),
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            outpos: 0,
            nick: None,
            user: None,
            realname: String::new(),
            registered: false,
            cap_negotiating: false,
            away: None,
            channels: HashSet::new(),
            connected_at: now,
            last_recv: now,
            signon: 0,
            idle_since: 0,
            awaiting_pong: false,
            ping_sent: now,
            quit: None,
        }
    }

    fn push_line(&mut self, line: &str) {
        if self.quit.is_some() {
            return;
        }
        let bytes = line.as_bytes();
        let mut end = bytes.len().min(LINE_MAX);
        while end < bytes.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        if self.outbuf.len() - self.outpos + end + 2 > OUTBUF_MAX {
            self.quit = Some("SendQ exceeded".into());
            return;
        }
        self.outbuf.extend_from_slice(&bytes[..end]);
        self.outbuf.extend_from_slice(b"\r\n");
    }
}

#[derive(Default, Clone, Copy)]
struct Member {
    op: bool,
    voice: bool,
}

struct Channel {
    name: String, // display case
    created: u64,
    topic: Option<String>,
    topic_by: String,
    topic_at: u64,
    key: Option<String>,
    limit: Option<usize>,
    no_external: bool, // +n
    topic_locked: bool, // +t
    members: HashMap<usize, Member>,
}

impl Channel {
    fn new(name: &str) -> Self {
        Channel {
            name: name.to_string(),
            created: unix_now(),
            topic: None,
            topic_by: String::new(),
            topic_at: 0,
            key: None,
            limit: None,
            no_external: true,
            topic_locked: true,
            members: HashMap::new(),
        }
    }

    fn mode_string(&self, with_args: bool) -> String {
        let mut modes = String::from("+");
        let mut args = Vec::new();
        if self.no_external {
            modes.push('n');
        }
        if self.topic_locked {
            modes.push('t');
        }
        if let Some(k) = &self.key {
            modes.push('k');
            args.push(if with_args { k.clone() } else { "*".into() });
        }
        if let Some(l) = self.limit {
            modes.push('l');
            args.push(l.to_string());
        }
        if args.is_empty() {
            modes
        } else {
            format!("{} {}", modes, args.join(" "))
        }
    }
}

// One validated channel-mode change, computed read-only then applied.
enum ModeChange {
    Flag(char, bool),                    // t / n
    Key(bool, Option<String>),           // +k key / -k
    Limit(bool, Option<usize>),          // +l n / -l
    Priv(char, bool, usize, String),     // o / v on (target cid, target nick)
}

pub struct Server {
    conns: HashMap<usize, Conn>,
    nicks: HashMap<String, usize>,      // lower(nick) -> cid
    channels: HashMap<String, Channel>, // lower(name) -> channel
    next_cid: usize,
    created: u64,
}

impl Server {
    pub fn new() -> Self {
        Server {
            conns: HashMap::new(),
            nicks: HashMap::new(),
            channels: HashMap::new(),
            next_cid: 1,
            created: unix_now(),
        }
    }

    // ---- lifecycle ---------------------------------------------------------

    pub fn accept(&mut self, stream: TcpStream, addr: SocketAddr) {
        if self.conns.len() >= MAX_CLIENTS {
            let mut s = stream;
            let _ = s.write_all(b"ERROR :Closing Link: server is full\r\n");
            return;
        }
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);
        let cid = self.next_cid;
        self.next_cid += 1;
        self.conns.insert(cid, Conn::new(stream, addr));
        println!("[conn {}] connected from {} ({} online)", cid, addr.ip(), self.conns.len());
        self.send(
            cid,
            &format!(":{} NOTICE * :*** Welcome to {}; please register (NICK/USER)", SERVER_NAME, NETWORK),
        );
    }

    /// Read from every connection, dispatch complete lines. Returns true if
    /// any input was processed (lets the main loop shorten its sleep).
    pub fn pump(&mut self) -> bool {
        let cids: Vec<usize> = self.conns.keys().copied().collect();
        let mut busy = false;
        for cid in cids {
            let mut lines: Vec<String> = Vec::new();
            {
                let Some(c) = self.conns.get_mut(&cid) else { continue };
                if c.quit.is_some() {
                    continue;
                }
                let mut buf = [0u8; READ_CHUNK];
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        c.quit = Some("Connection closed".into());
                        continue;
                    }
                    Ok(n) => {
                        c.inbuf.extend_from_slice(&buf[..n]);
                        if c.inbuf.len() > INBUF_MAX {
                            c.quit = Some("Excess flood".into());
                            continue;
                        }
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                    Err(e) => {
                        c.quit = Some(format!("Read error: {}", e.kind()));
                        continue;
                    }
                }
                while let Some(pos) = c.inbuf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = c.inbuf.drain(..=pos).collect();
                    let mut line = String::from_utf8_lossy(&raw).into_owned();
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    if !line.is_empty() {
                        line.truncate_floor(LINE_MAX);
                        lines.push(line);
                    }
                }
            }
            for line in lines {
                busy = true;
                if self.conns.get(&cid).map_or(true, |c| c.quit.is_some()) {
                    break;
                }
                self.handle_line(cid, &line);
            }
        }
        busy
    }

    /// Timers: registration timeout, ping/pong liveness.
    pub fn tick(&mut self) {
        enum Act {
            Kill(String),
            Ping,
        }
        let mut acts: Vec<(usize, Act)> = Vec::new();
        for (&cid, c) in &self.conns {
            if c.quit.is_some() {
                continue;
            }
            if !c.registered && c.connected_at.elapsed() > REG_TIMEOUT {
                acts.push((cid, Act::Kill("Registration timeout".into())));
            } else if c.awaiting_pong && c.ping_sent.elapsed() > PONG_TIMEOUT {
                acts.push((cid, Act::Kill(format!("Ping timeout: {} seconds", PONG_TIMEOUT.as_secs()))));
            } else if !c.awaiting_pong && c.last_recv.elapsed() > PING_INTERVAL {
                acts.push((cid, Act::Ping));
            }
        }
        for (cid, act) in acts {
            match act {
                Act::Kill(reason) => self.kill(cid, &reason),
                Act::Ping => {
                    self.send(cid, &format!("PING :{}", SERVER_NAME));
                    if let Some(c) = self.conns.get_mut(&cid) {
                        c.awaiting_pong = true;
                        c.ping_sent = Instant::now();
                    }
                }
            }
        }
    }

    /// Drain outbufs (non-blocking; keeps the remainder on WouldBlock).
    pub fn flush(&mut self) {
        for c in self.conns.values_mut() {
            while c.outpos < c.outbuf.len() {
                match c.stream.write(&c.outbuf[c.outpos..]) {
                    Ok(0) => {
                        if c.quit.is_none() {
                            c.quit = Some("Write error".into());
                        }
                        break;
                    }
                    Ok(n) => c.outpos += n,
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                    Err(e) => {
                        if c.quit.is_none() {
                            c.quit = Some(format!("Write error: {}", e.kind()));
                        }
                        break;
                    }
                }
            }
            if c.outpos > 0 && c.outpos == c.outbuf.len() {
                c.outbuf.clear();
                c.outpos = 0;
            }
        }
    }

    /// Remove connections whose `quit` is set: broadcast QUIT to channel
    /// peers, free the nick, best-effort deliver an ERROR line, close.
    pub fn reap(&mut self) {
        let dead: Vec<(usize, String)> = self
            .conns
            .iter()
            .filter_map(|(&cid, c)| c.quit.clone().map(|r| (cid, r)))
            .collect();
        for (cid, reason) in dead {
            let prefix = self.prefix(cid);
            let registered = self.conns.get(&cid).map_or(false, |c| c.registered);
            let peers = self.channel_peers(cid, false);
            let chans: Vec<String> = self
                .conns
                .get(&cid)
                .map(|c| c.channels.iter().cloned().collect())
                .unwrap_or_default();
            for ch in chans {
                if let Some(chan) = self.channels.get_mut(&ch) {
                    chan.members.remove(&cid);
                    if chan.members.is_empty() {
                        self.channels.remove(&ch);
                    }
                }
            }
            if registered {
                let line = format!(":{} QUIT :{}", prefix, reason);
                for p in peers {
                    self.send(p, &line);
                }
            }
            if let Some(mut c) = self.conns.remove(&cid) {
                if let Some(n) = &c.nick {
                    self.nicks.remove(&message::lower(n));
                }
                let err = format!("ERROR :Closing Link: {} ({})\r\n", c.host, reason);
                if c.outpos < c.outbuf.len() {
                    let _ = c.stream.write(&c.outbuf[c.outpos..]);
                }
                let _ = c.stream.write(err.as_bytes());
                let _ = c.stream.shutdown(std::net::Shutdown::Both);
                println!("[conn {}] gone: {} ({} online)", cid, reason, self.conns.len());
            }
        }
    }

    // ---- plumbing ----------------------------------------------------------

    fn send(&mut self, cid: usize, line: &str) {
        if let Some(c) = self.conns.get_mut(&cid) {
            c.push_line(line);
        }
    }

    fn num(&mut self, cid: usize, code: &str, rest: &str) {
        let nick = self
            .conns
            .get(&cid)
            .and_then(|c| c.nick.clone())
            .unwrap_or_else(|| "*".into());
        self.send(cid, &format!(":{} {} {} {}", SERVER_NAME, code, nick, rest));
    }

    fn prefix(&self, cid: usize) -> String {
        match self.conns.get(&cid) {
            Some(c) => format!(
                "{}!{}@{}",
                c.nick.as_deref().unwrap_or("*"),
                c.user.as_deref().unwrap_or("unknown"),
                c.host
            ),
            None => "*".into(),
        }
    }

    fn kill(&mut self, cid: usize, reason: &str) {
        if let Some(c) = self.conns.get_mut(&cid) {
            if c.quit.is_none() {
                c.quit = Some(reason.to_string());
            }
        }
    }

    /// Everyone sharing at least one channel with `cid` (deduplicated).
    fn channel_peers(&self, cid: usize, include_self: bool) -> Vec<usize> {
        let mut set: HashSet<usize> = HashSet::new();
        if let Some(c) = self.conns.get(&cid) {
            for ch in &c.channels {
                if let Some(chan) = self.channels.get(ch) {
                    set.extend(chan.members.keys().copied());
                }
            }
        }
        if include_self {
            set.insert(cid);
        } else {
            set.remove(&cid);
        }
        set.into_iter().collect()
    }

    fn bcast_channel(&mut self, chan_l: &str, line: &str, except: Option<usize>) {
        let members: Vec<usize> = self
            .channels
            .get(chan_l)
            .map(|ch| ch.members.keys().copied().collect())
            .unwrap_or_default();
        for m in members {
            if Some(m) != except {
                self.send(m, line);
            }
        }
    }

    fn nick_of(&self, cid: usize) -> String {
        self.conns
            .get(&cid)
            .and_then(|c| c.nick.clone())
            .unwrap_or_else(|| "*".into())
    }

    // ---- dispatch ----------------------------------------------------------

    pub fn handle_line(&mut self, cid: usize, line: &str) {
        let Some(m) = message::parse(line) else { return };
        if let Some(c) = self.conns.get_mut(&cid) {
            c.last_recv = Instant::now();
            c.awaiting_pong = false;
        }
        let registered = self.conns.get(&cid).map_or(false, |c| c.registered);
        let p = &m.params;
        match m.cmd.as_str() {
            "CAP" => self.cmd_cap(cid, p),
            "PASS" => {} // no server password; accepted and ignored
            "NICK" => self.cmd_nick(cid, p),
            "USER" => self.cmd_user(cid, p),
            "PING" => match p.first() {
                Some(tok) => self.send(cid, &format!(":{0} PONG {0} :{1}", SERVER_NAME, tok)),
                None => self.num(cid, "409", ":No origin specified"),
            },
            "PONG" => {} // liveness already noted above
            "QUIT" => {
                let reason = p.first().cloned().unwrap_or_else(|| "Client quit".into());
                self.kill(cid, &format!("Quit: {}", reason));
            }
            _ if !registered => self.num(cid, "451", ":You have not registered"),
            "JOIN" => self.cmd_join(cid, p),
            "PART" => self.cmd_part(cid, p),
            "PRIVMSG" => self.cmd_msg(cid, p, true),
            "NOTICE" => self.cmd_msg(cid, p, false),
            "TOPIC" => self.cmd_topic(cid, p),
            "NAMES" => self.cmd_names(cid, p),
            "LIST" => self.cmd_list(cid, p),
            "WHO" => self.cmd_who(cid, p),
            "WHOIS" => self.cmd_whois(cid, p),
            "MODE" => self.cmd_mode(cid, p),
            "KICK" => self.cmd_kick(cid, p),
            "INVITE" => self.cmd_invite(cid, p),
            "AWAY" => self.cmd_away(cid, p),
            "MOTD" => self.cmd_motd(cid),
            "LUSERS" => self.cmd_lusers(cid),
            "ISON" => self.cmd_ison(cid, p),
            "USERHOST" => self.cmd_userhost(cid, p),
            "VERSION" => {
                self.num(cid, "351", &format!("{} {} :wasm32-wasip2, single-threaded", VERSION_STR, SERVER_NAME));
            }
            "TIME" => {
                let t = unix_now();
                self.num(cid, "391", &format!("{} :unix time {}", SERVER_NAME, t));
            }
            other => self.num(cid, "421", &format!("{} :Unknown command", other)),
        }
    }

    // ---- registration ------------------------------------------------------

    fn cmd_cap(&mut self, cid: usize, p: &[String]) {
        let sub = p.first().map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        let nick = self.nick_of(cid);
        match sub.as_str() {
            // No capabilities are offered; modern clients see the empty set,
            // request nothing (or get NAKed), then CAP END and register.
            "LS" | "LIST" => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    if !c.registered {
                        c.cap_negotiating = true;
                    }
                }
                self.send(cid, &format!(":{} CAP {} {} :", SERVER_NAME, nick, sub));
            }
            "REQ" => {
                let caps = p.get(1).cloned().unwrap_or_default();
                self.send(cid, &format!(":{} CAP {} NAK :{}", SERVER_NAME, nick, caps));
            }
            "END" => {
                if let Some(c) = self.conns.get_mut(&cid) {
                    c.cap_negotiating = false;
                }
                self.try_register(cid);
            }
            other => self.num(cid, "410", &format!("{} :Invalid CAP command", other)),
        }
    }

    fn cmd_nick(&mut self, cid: usize, p: &[String]) {
        let Some(newnick) = p.first() else {
            return self.num(cid, "431", ":No nickname given");
        };
        if !message::valid_nick(newnick) {
            return self.num(cid, "432", &format!("{} :Erroneous nickname", newnick));
        }
        let lower = message::lower(newnick);
        if let Some(&owner) = self.nicks.get(&lower) {
            if owner != cid {
                return self.num(cid, "433", &format!("{} :Nickname is already in use", newnick));
            }
        }
        let old_prefix = self.prefix(cid);
        let (was_registered, old_lower) = match self.conns.get(&cid) {
            Some(c) => (c.registered, c.nick.as_deref().map(message::lower)),
            None => return,
        };
        if let Some(ol) = old_lower {
            self.nicks.remove(&ol);
        }
        self.nicks.insert(lower, cid);
        if let Some(c) = self.conns.get_mut(&cid) {
            c.nick = Some(newnick.clone());
        }
        if was_registered {
            let line = format!(":{} NICK :{}", old_prefix, newnick);
            for peer in self.channel_peers(cid, true) {
                self.send(peer, &line);
            }
        } else {
            self.try_register(cid);
        }
    }

    fn cmd_user(&mut self, cid: usize, p: &[String]) {
        if self.conns.get(&cid).map_or(false, |c| c.registered) {
            return self.num(cid, "462", ":You may not reregister");
        }
        if p.len() < 4 {
            return self.num(cid, "461", "USER :Not enough parameters");
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            let mut user: String = p[0].chars().filter(|c| c.is_ascii_graphic()).take(12).collect();
            if user.is_empty() {
                user.push('u');
            }
            c.user = Some(user);
            c.realname = p[3].chars().take(64).collect();
        }
        self.try_register(cid);
    }

    fn try_register(&mut self, cid: usize) {
        let ready = self
            .conns
            .get(&cid)
            .map_or(false, |c| !c.registered && !c.cap_negotiating && c.nick.is_some() && c.user.is_some());
        if !ready {
            return;
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            c.registered = true;
            c.signon = unix_now();
            c.idle_since = c.signon;
        }
        println!("[conn {}] registered as {}", cid, self.nick_of(cid));
        let prefix = self.prefix(cid);
        self.num(cid, "001", &format!(":Welcome to the {} IRC Network, {}", NETWORK, prefix));
        self.num(cid, "002", &format!(":Your host is {}, running version {}", SERVER_NAME, VERSION_STR));
        self.num(cid, "003", &format!(":This server was created at unix time {}", self.created));
        self.num(cid, "004", &format!("{} {} o ovtnkl", SERVER_NAME, VERSION_STR));
        self.num(
            cid,
            "005",
            &format!(
                "NETWORK={} CASEMAPPING=ascii CHANTYPES=# PREFIX=(ov)@+ CHANMODES=,k,l,nt NICKLEN={} CHANNELLEN={} TOPICLEN={} :are supported by this server",
                NETWORK, NICK_MAX, CHANNEL_MAX, TOPIC_MAX
            ),
        );
        self.cmd_lusers(cid);
        self.cmd_motd(cid);
    }

    fn cmd_lusers(&mut self, cid: usize) {
        let users = self.conns.values().filter(|c| c.registered).count();
        let chans = self.channels.len();
        self.num(cid, "251", &format!(":There are {} users and 0 invisible on 1 servers", users));
        self.num(cid, "254", &format!("{} :channels formed", chans));
        self.num(cid, "255", &format!(":I have {} clients and 0 servers", users));
    }

    fn cmd_motd(&mut self, cid: usize) {
        self.num(cid, "375", &format!(":- {} Message of the day -", SERVER_NAME));
        for l in [
            "- Welcome to Enclave IRC: an IRC server compiled to WebAssembly,",
            "- running as a wasi:sockets service inside a confidential",
            "- compute enclave on the Enclave hosting platform.",
            "-",
            "- Everything here is ephemeral by design: no disk, no logs of",
            "- your messages, no history. When the deployment ends, the",
            "- network vanishes. Be kind while it lasts.",
        ] {
            self.num(cid, "372", &format!(":{}", l));
        }
        self.num(cid, "376", ":End of /MOTD command.");
    }

    // ---- channels ----------------------------------------------------------

    fn cmd_join(&mut self, cid: usize, p: &[String]) {
        let Some(list) = p.first() else {
            return self.num(cid, "461", "JOIN :Not enough parameters");
        };
        if list == "0" {
            let chans: Vec<String> = self
                .conns
                .get(&cid)
                .map(|c| c.channels.iter().cloned().collect())
                .unwrap_or_default();
            for ch in chans {
                let display = self.channels.get(&ch).map(|c| c.name.clone()).unwrap_or(ch.clone());
                self.do_part(cid, &display, "Left all channels");
            }
            return;
        }
        let keys: Vec<&str> = p.get(1).map(|k| k.split(',').collect()).unwrap_or_default();
        for (i, name) in list.split(',').take(12).enumerate() {
            let name = name.trim();
            if !message::valid_channel(name) {
                self.num(cid, "403", &format!("{} :No such channel", name));
                continue;
            }
            let lower = message::lower(name);
            let already = self.conns.get(&cid).map_or(false, |c| c.channels.contains(&lower));
            if already {
                continue;
            }
            let nchans = self.conns.get(&cid).map_or(0, |c| c.channels.len());
            if nchans >= CHANS_PER_USER {
                self.num(cid, "405", &format!("{} :You have joined too many channels", name));
                continue;
            }
            let is_new = !self.channels.contains_key(&lower);
            if !is_new {
                let chan = self.channels.get(&lower).unwrap();
                if let Some(k) = &chan.key {
                    if keys.get(i).copied() != Some(k.as_str()) {
                        self.num(cid, "475", &format!("{} :Cannot join channel (+k)", chan.name));
                        continue;
                    }
                }
                if let Some(l) = chan.limit {
                    if chan.members.len() >= l {
                        self.num(cid, "471", &format!("{} :Cannot join channel (+l)", chan.name));
                        continue;
                    }
                }
            }
            let chan = self.channels.entry(lower.clone()).or_insert_with(|| Channel::new(name));
            let display = chan.name.clone();
            chan.members.insert(cid, Member { op: is_new, voice: false });
            if let Some(c) = self.conns.get_mut(&cid) {
                c.channels.insert(lower.clone());
            }
            let prefix = self.prefix(cid);
            self.bcast_channel(&lower, &format!(":{} JOIN :{}", prefix, display), None);
            let topic_info = self
                .channels
                .get(&lower)
                .and_then(|c| c.topic.clone().map(|t| (t, c.topic_by.clone(), c.topic_at)));
            if let Some((t, by, at)) = topic_info {
                self.num(cid, "332", &format!("{} :{}", display, t));
                self.num(cid, "333", &format!("{} {} {}", display, by, at));
            }
            self.names_reply(cid, &lower);
        }
    }

    fn do_part(&mut self, cid: usize, display: &str, reason: &str) {
        let lower = message::lower(display);
        let prefix = self.prefix(cid);
        self.bcast_channel(&lower, &format!(":{} PART {} :{}", prefix, display, reason), None);
        if let Some(chan) = self.channels.get_mut(&lower) {
            chan.members.remove(&cid);
            if chan.members.is_empty() {
                self.channels.remove(&lower);
            }
        }
        if let Some(c) = self.conns.get_mut(&cid) {
            c.channels.remove(&lower);
        }
    }

    fn cmd_part(&mut self, cid: usize, p: &[String]) {
        let Some(list) = p.first() else {
            return self.num(cid, "461", "PART :Not enough parameters");
        };
        let nick = self.nick_of(cid);
        let reason = p.get(1).cloned().unwrap_or(nick);
        for name in list.clone().split(',').take(12) {
            let lower = message::lower(name);
            let Some(chan) = self.channels.get(&lower) else {
                self.num(cid, "403", &format!("{} :No such channel", name));
                continue;
            };
            let display = chan.name.clone();
            if !chan.members.contains_key(&cid) {
                self.num(cid, "442", &format!("{} :You're not on that channel", display));
                continue;
            }
            self.do_part(cid, &display, &reason);
        }
    }

    fn cmd_msg(&mut self, cid: usize, p: &[String], is_privmsg: bool) {
        let cmd = if is_privmsg { "PRIVMSG" } else { "NOTICE" };
        let Some(targets) = p.first() else {
            if is_privmsg {
                self.num(cid, "411", &format!(":No recipient given ({})", cmd));
            }
            return;
        };
        let Some(text) = p.get(1).filter(|t| !t.is_empty()) else {
            if is_privmsg {
                self.num(cid, "412", ":No text to send");
            }
            return;
        };
        let prefix = self.prefix(cid);
        if let Some(c) = self.conns.get_mut(&cid) {
            c.idle_since = unix_now();
        }
        for target in targets.clone().split(',').take(MSG_TARGETS_MAX) {
            if target.starts_with('#') {
                let lower = message::lower(target);
                let Some(chan) = self.channels.get(&lower) else {
                    if is_privmsg {
                        self.num(cid, "403", &format!("{} :No such channel", target));
                    }
                    continue;
                };
                let display = chan.name.clone();
                if chan.no_external && !chan.members.contains_key(&cid) {
                    if is_privmsg {
                        self.num(cid, "404", &format!("{} :Cannot send to channel", display));
                    }
                    continue;
                }
                self.bcast_channel(&lower, &format!(":{} {} {} :{}", prefix, cmd, display, text), Some(cid));
            } else {
                let Some(&tcid) = self.nicks.get(&message::lower(target)) else {
                    if is_privmsg {
                        self.num(cid, "401", &format!("{} :No such nick/channel", target));
                    }
                    continue;
                };
                let tnick = self.nick_of(tcid);
                self.send(tcid, &format!(":{} {} {} :{}", prefix, cmd, tnick, text));
                if is_privmsg {
                    if let Some(msg) = self.conns.get(&tcid).and_then(|c| c.away.clone()) {
                        self.num(cid, "301", &format!("{} :{}", tnick, msg));
                    }
                }
            }
        }
    }

    fn cmd_topic(&mut self, cid: usize, p: &[String]) {
        let Some(name) = p.first() else {
            return self.num(cid, "461", "TOPIC :Not enough parameters");
        };
        let lower = message::lower(name);
        let Some(chan) = self.channels.get(&lower) else {
            return self.num(cid, "403", &format!("{} :No such channel", name));
        };
        let display = chan.name.clone();
        if p.len() < 2 {
            let info = chan.topic.clone().map(|t| (t, chan.topic_by.clone(), chan.topic_at));
            return match info {
                Some((t, by, at)) => {
                    self.num(cid, "332", &format!("{} :{}", display, t));
                    self.num(cid, "333", &format!("{} {} {}", display, by, at));
                }
                None => self.num(cid, "331", &format!("{} :No topic is set", display)),
            };
        }
        let member = chan.members.get(&cid).copied();
        let Some(member) = member else {
            return self.num(cid, "442", &format!("{} :You're not on that channel", display));
        };
        if chan.topic_locked && !member.op {
            return self.num(cid, "482", &format!("{} :You're not channel operator", display));
        }
        let mut new_topic: String = p[1].clone();
        new_topic.truncate_floor(TOPIC_MAX);
        let nick = self.nick_of(cid);
        if let Some(chan) = self.channels.get_mut(&lower) {
            chan.topic = if new_topic.is_empty() { None } else { Some(new_topic.clone()) };
            chan.topic_by = nick;
            chan.topic_at = unix_now();
        }
        let prefix = self.prefix(cid);
        self.bcast_channel(&lower, &format!(":{} TOPIC {} :{}", prefix, display, new_topic), None);
    }

    fn names_reply(&mut self, cid: usize, chan_l: &str) {
        let Some(chan) = self.channels.get(chan_l) else {
            return self.num(cid, "366", &format!("{} :End of /NAMES list", chan_l));
        };
        let display = chan.name.clone();
        let mut names: Vec<String> = Vec::new();
        for (&mcid, m) in &chan.members {
            let sigil = if m.op { "@" } else if m.voice { "+" } else { "" };
            names.push(format!("{}{}", sigil, self.nick_of(mcid)));
        }
        names.sort();
        let mut line = String::new();
        let mut chunks: Vec<String> = Vec::new();
        for n in names {
            if line.len() + n.len() + 1 > 380 {
                chunks.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&n);
        }
        if !line.is_empty() {
            chunks.push(line);
        }
        for chunk in chunks {
            self.num(cid, "353", &format!("= {} :{}", display, chunk));
        }
        self.num(cid, "366", &format!("{} :End of /NAMES list", display));
    }

    fn cmd_names(&mut self, cid: usize, p: &[String]) {
        match p.first() {
            Some(list) => {
                for name in list.clone().split(',').take(12) {
                    self.names_reply(cid, &message::lower(name));
                }
            }
            None => self.num(cid, "366", "* :End of /NAMES list"),
        }
    }

    fn cmd_list(&mut self, cid: usize, p: &[String]) {
        self.num(cid, "321", "Channel :Users Name");
        let wanted: Option<HashSet<String>> = p
            .first()
            .map(|l| l.split(',').map(message::lower).collect());
        let rows: Vec<(String, usize, String)> = self
            .channels
            .iter()
            .filter(|(l, _)| wanted.as_ref().map_or(true, |w| w.contains(*l)))
            .map(|(_, c)| (c.name.clone(), c.members.len(), c.topic.clone().unwrap_or_default()))
            .collect();
        for (name, n, topic) in rows {
            self.num(cid, "322", &format!("{} {} :{}", name, n, topic));
        }
        self.num(cid, "323", ":End of /LIST");
    }

    fn who_line(&mut self, cid: usize, chan_display: &str, tcid: usize) {
        let Some(t) = self.conns.get(&tcid) else { return };
        let flags = format!(
            "{}{}",
            if t.away.is_some() { "G" } else { "H" },
            {
                let lower = message::lower(chan_display);
                match self.channels.get(&lower).and_then(|c| c.members.get(&tcid)) {
                    Some(m) if m.op => "@",
                    Some(m) if m.voice => "+",
                    _ => "",
                }
            }
        );
        let (user, host, nick, real) = {
            let t = self.conns.get(&tcid).unwrap();
            (
                t.user.clone().unwrap_or_else(|| "unknown".into()),
                t.host.clone(),
                t.nick.clone().unwrap_or_else(|| "*".into()),
                t.realname.clone(),
            )
        };
        self.num(
            cid,
            "352",
            &format!("{} {} {} {} {} {} :0 {}", chan_display, user, host, SERVER_NAME, nick, flags, real),
        );
    }

    fn cmd_who(&mut self, cid: usize, p: &[String]) {
        let mask = p.first().cloned().unwrap_or_else(|| "*".into());
        if mask.starts_with('#') {
            let lower = message::lower(&mask);
            if let Some(chan) = self.channels.get(&lower) {
                let display = chan.name.clone();
                let members: Vec<usize> = chan.members.keys().copied().collect();
                for m in members {
                    self.who_line(cid, &display, m);
                }
            }
        } else if let Some(&tcid) = self.nicks.get(&message::lower(&mask)) {
            self.who_line(cid, "*", tcid);
        }
        self.num(cid, "315", &format!("{} :End of /WHO list", mask));
    }

    fn cmd_whois(&mut self, cid: usize, p: &[String]) {
        // "WHOIS server nick" form: the last param is the nick we care about.
        let Some(target) = p.last().filter(|s| !s.is_empty()) else {
            return self.num(cid, "431", ":No nickname given");
        };
        let Some(&tcid) = self.nicks.get(&message::lower(target)) else {
            self.num(cid, "401", &format!("{} :No such nick/channel", target));
            return self.num(cid, "318", &format!("{} :End of /WHOIS list", target));
        };
        let (nick, user, host, real, away, signon, idle_since, tchans) = {
            let t = self.conns.get(&tcid).unwrap();
            (
                t.nick.clone().unwrap_or_else(|| "*".into()),
                t.user.clone().unwrap_or_else(|| "unknown".into()),
                t.host.clone(),
                t.realname.clone(),
                t.away.clone(),
                t.signon,
                t.idle_since,
                t.channels.clone(),
            )
        };
        self.num(cid, "311", &format!("{} {} {} * :{}", nick, user, host, real));
        let mut chan_names: Vec<String> = Vec::new();
        for ch in &tchans {
            if let Some(chan) = self.channels.get(ch) {
                let sigil = match chan.members.get(&tcid) {
                    Some(m) if m.op => "@",
                    Some(m) if m.voice => "+",
                    _ => "",
                };
                chan_names.push(format!("{}{}", sigil, chan.name));
            }
        }
        if !chan_names.is_empty() {
            chan_names.sort();
            self.num(cid, "319", &format!("{} :{}", nick, chan_names.join(" ")));
        }
        self.num(cid, "312", &format!("{} {} :{} (wasm enclave)", nick, SERVER_NAME, NETWORK));
        if let Some(a) = away {
            self.num(cid, "301", &format!("{} :{}", nick, a));
        }
        let idle = unix_now().saturating_sub(idle_since);
        self.num(cid, "317", &format!("{} {} {} :seconds idle, signon time", nick, idle, signon));
        self.num(cid, "318", &format!("{} :End of /WHOIS list", nick));
    }

    // ---- modes -------------------------------------------------------------

    fn cmd_mode(&mut self, cid: usize, p: &[String]) {
        let Some(target) = p.first() else {
            return self.num(cid, "461", "MODE :Not enough parameters");
        };
        if !target.starts_with('#') {
            let self_nick = self.nick_of(cid);
            if message::lower(target) != message::lower(&self_nick) {
                return self.num(cid, "502", ":Cannot change mode for other users");
            }
            return match p.get(1) {
                None => self.num(cid, "221", "+"),
                Some(_) => self.num(cid, "501", ":Unknown MODE flag"),
            };
        }
        let lower = message::lower(target);
        let Some(chan) = self.channels.get(&lower) else {
            return self.num(cid, "403", &format!("{} :No such channel", target));
        };
        let display = chan.name.clone();
        let is_member = chan.members.contains_key(&cid);
        let is_op = chan.members.get(&cid).map_or(false, |m| m.op);

        let Some(modestr) = p.get(1) else {
            let modes = chan.mode_string(is_member);
            let created = chan.created;
            self.num(cid, "324", &format!("{} {}", display, modes));
            self.num(cid, "329", &format!("{} {}", display, created));
            return;
        };

        // Ban-list probe ("MODE #chan b" / "+b" with no mask): reply with an
        // empty list instead of an error; clients do this on every join.
        if p.len() == 2 && modestr.trim_matches('+') == "b" {
            return self.num(cid, "368", &format!("{} :End of Channel Ban List", display));
        }
        if !is_op {
            return self.num(cid, "482", &format!("{} :You're not channel operator", display));
        }

        // Phase 1 (read-only): validate every requested change.
        let mut changes: Vec<ModeChange> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        let mut adding = true;
        let mut argi = 2;
        let next_arg = |argi: &mut usize| -> Option<String> {
            let a = p.get(*argi).cloned();
            *argi += 1;
            a
        };
        for mc in modestr.chars() {
            match mc {
                '+' => adding = true,
                '-' => adding = false,
                't' => changes.push(ModeChange::Flag('t', adding)),
                'n' => changes.push(ModeChange::Flag('n', adding)),
                'k' => {
                    if adding {
                        match next_arg(&mut argi) {
                            Some(k) if !k.is_empty() => changes.push(ModeChange::Key(true, Some(k))),
                            _ => errors.push(("461".into(), "MODE :Not enough parameters".into())),
                        }
                    } else {
                        changes.push(ModeChange::Key(false, None));
                    }
                }
                'l' => {
                    if adding {
                        match next_arg(&mut argi).and_then(|a| a.parse::<usize>().ok()).filter(|&n| n > 0) {
                            Some(n) => changes.push(ModeChange::Limit(true, Some(n))),
                            None => errors.push(("461".into(), "MODE :Not enough parameters".into())),
                        }
                    } else {
                        changes.push(ModeChange::Limit(false, None));
                    }
                }
                'o' | 'v' => match next_arg(&mut argi) {
                    Some(tnick) => match self.nicks.get(&message::lower(&tnick)) {
                        Some(&tcid) if self.channels.get(&lower).map_or(false, |c| c.members.contains_key(&tcid)) => {
                            changes.push(ModeChange::Priv(mc, adding, tcid, tnick));
                        }
                        _ => errors.push(("441".into(), format!("{} {} :They aren't on that channel", tnick, display))),
                    },
                    None => errors.push(("461".into(), "MODE :Not enough parameters".into())),
                },
                other => errors.push(("472".into(), format!("{} :is unknown mode char to me", other))),
            }
        }
        for (code, text) in errors {
            self.num(cid, &code, &text);
        }
        if changes.is_empty() {
            return;
        }

        // Phase 2: apply and build the broadcast string.
        let mut out_modes = String::new();
        let mut out_args: Vec<String> = Vec::new();
        let mut last_sign: Option<bool> = None;
        let mut push_mode = |out_modes: &mut String, sign: bool, c: char| {
            if last_sign != Some(sign) {
                out_modes.push(if sign { '+' } else { '-' });
                last_sign = Some(sign);
            }
            out_modes.push(c);
        };
        {
            let chan = self.channels.get_mut(&lower).unwrap();
            for ch in &changes {
                match ch {
                    ModeChange::Flag(c, add) => {
                        match c {
                            't' => chan.topic_locked = *add,
                            _ => chan.no_external = *add,
                        }
                        push_mode(&mut out_modes, *add, *c);
                    }
                    ModeChange::Key(add, k) => {
                        chan.key = k.clone();
                        push_mode(&mut out_modes, *add, 'k');
                        if let Some(k) = k {
                            out_args.push(k.clone());
                        }
                    }
                    ModeChange::Limit(add, l) => {
                        chan.limit = *l;
                        push_mode(&mut out_modes, *add, 'l');
                        if let Some(l) = l {
                            out_args.push(l.to_string());
                        }
                    }
                    ModeChange::Priv(c, add, tcid, tnick) => {
                        if let Some(m) = chan.members.get_mut(tcid) {
                            match c {
                                'o' => m.op = *add,
                                _ => m.voice = *add,
                            }
                        }
                        push_mode(&mut out_modes, *add, *c);
                        out_args.push(tnick.clone());
                    }
                }
            }
        }
        let prefix = self.prefix(cid);
        let tail = if out_args.is_empty() {
            out_modes
        } else {
            format!("{} {}", out_modes, out_args.join(" "))
        };
        self.bcast_channel(&lower, &format!(":{} MODE {} {}", prefix, display, tail), None);
    }

    fn cmd_kick(&mut self, cid: usize, p: &[String]) {
        if p.len() < 2 {
            return self.num(cid, "461", "KICK :Not enough parameters");
        }
        let lower = message::lower(&p[0]);
        let Some(chan) = self.channels.get(&lower) else {
            return self.num(cid, "403", &format!("{} :No such channel", p[0]));
        };
        let display = chan.name.clone();
        if !chan.members.contains_key(&cid) {
            return self.num(cid, "442", &format!("{} :You're not on that channel", display));
        }
        if !chan.members.get(&cid).map_or(false, |m| m.op) {
            return self.num(cid, "482", &format!("{} :You're not channel operator", display));
        }
        let Some(&tcid) = self.nicks.get(&message::lower(&p[1])) else {
            return self.num(cid, "401", &format!("{} :No such nick/channel", p[1]));
        };
        if !chan.members.contains_key(&tcid) {
            return self.num(cid, "441", &format!("{} {} :They aren't on that channel", p[1], display));
        }
        let kicker = self.nick_of(cid);
        let reason = p.get(2).cloned().unwrap_or(kicker);
        let tnick = self.nick_of(tcid);
        let prefix = self.prefix(cid);
        self.bcast_channel(&lower, &format!(":{} KICK {} {} :{}", prefix, display, tnick, reason), None);
        if let Some(chan) = self.channels.get_mut(&lower) {
            chan.members.remove(&tcid);
            if chan.members.is_empty() {
                self.channels.remove(&lower);
            }
        }
        if let Some(c) = self.conns.get_mut(&tcid) {
            c.channels.remove(&lower);
        }
    }

    fn cmd_invite(&mut self, cid: usize, p: &[String]) {
        if p.len() < 2 {
            return self.num(cid, "461", "INVITE :Not enough parameters");
        }
        let Some(&tcid) = self.nicks.get(&message::lower(&p[0])) else {
            return self.num(cid, "401", &format!("{} :No such nick/channel", p[0]));
        };
        let lower = message::lower(&p[1]);
        let Some(chan) = self.channels.get(&lower) else {
            return self.num(cid, "403", &format!("{} :No such channel", p[1]));
        };
        let display = chan.name.clone();
        if !chan.members.contains_key(&cid) {
            return self.num(cid, "442", &format!("{} :You're not on that channel", display));
        }
        if chan.members.contains_key(&tcid) {
            return self.num(cid, "443", &format!("{} {} :is already on channel", p[0], display));
        }
        let tnick = self.nick_of(tcid);
        self.num(cid, "341", &format!("{} {}", tnick, display));
        let prefix = self.prefix(cid);
        self.send(tcid, &format!(":{} INVITE {} :{}", prefix, tnick, display));
    }

    // ---- misc --------------------------------------------------------------

    fn cmd_away(&mut self, cid: usize, p: &[String]) {
        let msg = p.first().filter(|m| !m.is_empty()).cloned();
        let going_away = msg.is_some();
        if let Some(c) = self.conns.get_mut(&cid) {
            c.away = msg.map(|m| m.chars().take(200).collect());
        }
        if going_away {
            self.num(cid, "306", ":You have been marked as being away");
        } else {
            self.num(cid, "305", ":You are no longer marked as being away");
        }
    }

    fn cmd_ison(&mut self, cid: usize, p: &[String]) {
        let mut online: Vec<String> = Vec::new();
        for nick in p.iter().flat_map(|s| s.split_ascii_whitespace()).take(16) {
            if let Some(&tcid) = self.nicks.get(&message::lower(nick)) {
                online.push(self.nick_of(tcid));
            }
        }
        self.num(cid, "303", &format!(":{}", online.join(" ")));
    }

    fn cmd_userhost(&mut self, cid: usize, p: &[String]) {
        let mut out: Vec<String> = Vec::new();
        for nick in p.iter().take(5) {
            if let Some(&tcid) = self.nicks.get(&message::lower(nick)) {
                if let Some(t) = self.conns.get(&tcid) {
                    out.push(format!(
                        "{}={}{}@{}",
                        t.nick.as_deref().unwrap_or("*"),
                        if t.away.is_some() { "-" } else { "+" },
                        t.user.as_deref().unwrap_or("unknown"),
                        t.host
                    ));
                }
            }
        }
        self.num(cid, "302", &format!(":{}", out.join(" ")));
    }
}

/// `String::truncate` panics off a char boundary; this walks back to one.
trait TruncateFloor {
    fn truncate_floor(&mut self, max: usize);
}

impl TruncateFloor for String {
    fn truncate_floor(&mut self, max: usize) {
        if self.len() <= max {
            return;
        }
        let mut end = max;
        while end > 0 && !self.is_char_boundary(end) {
            end -= 1;
        }
        self.truncate(end);
    }
}
