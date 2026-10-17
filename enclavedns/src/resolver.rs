//! The validating resolver: iterative resolution from the root (or
//! forwarding to configured upstreams), a cache, and the DNSSEC chain of
//! trust from the root anchors down to every answer it hands out.
//!
//! `lookup(name, type)` answers for exactly one name (DNAME synthesis
//! included, CNAME chains not followed): the server follows chains itself,
//! because a hop may land in a hosted zone, in `.eth` or in `.onion`, none
//! of which must ever be asked of the internet.

use crate::dnssec::{self, Claim, Denial, Dnskey, Nsec, Nsec3, Rrsig};
use crate::name::Name;
use crate::net::{self, Dest, Udp};
use crate::tls::{self, Conn};
use crate::wire::{ede, rcode, rrsets, ty, Message, Record};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Sec {
    Secure,
    Insecure,
    Indeterminate,
    Bogus,
}

impl Sec {
    pub fn name(self) -> &'static str {
        match self {
            Sec::Secure => "secure",
            Sec::Insecure => "insecure",
            Sec::Indeterminate => "indeterminate",
            Sec::Bogus => "bogus",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Answer {
    pub rcode: u16,
    pub answer: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
    pub sec: Sec,
    pub ede: Option<(u16, String)>,
    /// Seconds this may be cached.
    pub ttl: u32,
    pub aa: bool,
    pub source: &'static str,
}

impl Answer {
    pub fn fail(code: u16, why: impl Into<String>) -> Answer {
        Answer {
            rcode: rcode::SERVFAIL,
            answer: Vec::new(),
            authority: Vec::new(),
            additional: Vec::new(),
            sec: Sec::Indeterminate,
            ede: Some((code, why.into())),
            ttl: 5,
            aa: false,
            source: "error",
        }
    }

    pub fn negative(rc: u16, ttl: u32, source: &'static str) -> Answer {
        Answer { rcode: rc, answer: Vec::new(), authority: Vec::new(), additional: Vec::new(), sec: Sec::Insecure, ede: None, ttl, aa: false, source }
    }

    /// The records of `rtype` owned by `name` in the answer.
    pub fn rrset(&self, name: &Name, rtype: u16) -> Vec<&Record> {
        self.answer.iter().filter(|r| r.rtype == rtype && r.name == *name).collect()
    }

    pub fn cname_target(&self, name: &Name) -> Option<Name> {
        self.answer.iter().find(|r| r.rtype == ty::CNAME && r.name == *name).and_then(|r| r.target())
    }
}

/// Upstream for forwarding mode.
#[derive(Clone, Debug)]
pub enum Upstream {
    Udp(SocketAddr),
    Tcp(SocketAddr),
    Tls(SocketAddr, String),
    Https(String),
}

impl Upstream {
    pub fn parse(s: &str) -> Result<Upstream, String> {
        let addr = |a: &str, port: u16| -> Result<SocketAddr, String> {
            a.parse::<SocketAddr>()
                .or_else(|_| a.trim_matches(['[', ']']).parse::<IpAddr>().map(|ip| SocketAddr::new(ip, port)))
                .map_err(|_| format!("upstream address must be an IP (port optional): {a}"))
        };
        if let Some(r) = s.strip_prefix("udp://") {
            Ok(Upstream::Udp(addr(r, 53)?))
        } else if let Some(r) = s.strip_prefix("tcp://") {
            Ok(Upstream::Tcp(addr(r, 53)?))
        } else if let Some(r) = s.strip_prefix("tls://") {
            // tls://1.1.1.1#cloudflare-dns.com
            let (a, n) = r.split_once('#').ok_or("tls:// upstreams need #<tls name>, e.g. tls://1.1.1.1#cloudflare-dns.com")?;
            Ok(Upstream::Tls(addr(a, 853)?, n.to_string()))
        } else if s.starts_with("https://") {
            tls::parse_url(s)?;
            Ok(Upstream::Https(s.to_string()))
        } else {
            Err(format!("unknown upstream {s} (udp://, tcp://, tls://ip#name, https://)"))
        }
    }

    pub fn text(&self) -> String {
        match self {
            Upstream::Udp(a) => format!("udp://{a}"),
            Upstream::Tcp(a) => format!("tcp://{a}"),
            Upstream::Tls(a, n) => format!("tls://{a}#{n}"),
            Upstream::Https(u) => u.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Auto,
    Udp,
    Tcp,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub forward: Vec<Upstream>,
    pub validate: bool,
    pub qname_minimisation: bool,
    pub transport: Transport,
    pub ipv4: bool,
    pub ipv6: bool,
    pub max_ttl: u32,
    pub max_negative_ttl: u32,
    pub cache_entries: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            forward: Vec::new(),
            validate: true,
            qname_minimisation: true,
            transport: Transport::Auto,
            ipv4: true,
            ipv6: true,
            max_ttl: 86400,
            max_negative_ttl: 3600,
            cache_entries: 50_000,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub upstream_queries: u64,
    pub upstream_udp: u64,
    pub upstream_tcp: u64,
    pub upstream_timeouts: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub secure: u64,
    pub insecure: u64,
    pub bogus: u64,
}

/// Per-request limits shared by every sub-lookup it causes.
pub struct Ctx {
    pub deadline_ms: u64,
    pub queries: Cell<u32>,
}

impl Ctx {
    pub fn new(budget: Duration) -> Rc<Ctx> {
        Rc::new(Ctx { deadline_ms: net::now_ms() + budget.as_millis() as u64, queries: Cell::new(0) })
    }

    fn left(&self) -> Duration {
        Duration::from_millis(self.deadline_ms.saturating_sub(net::now_ms()))
    }

    fn spend(&self) -> bool {
        let q = self.queries.get() + 1;
        self.queries.set(q);
        q <= MAX_QUERIES_PER_REQUEST && net::now_ms() < self.deadline_ms
    }
}

const MAX_QUERIES_PER_REQUEST: u32 = 150;
const MAX_DEPTH: u32 = 12;

#[derive(Clone)]
enum KeyState {
    Secure(Rc<Vec<Dnskey>>),
    Insecure,
    Bogus(u16, String),
}

struct Delegation {
    zone: Name,
    ns: Vec<Name>,
    addrs: RefCell<Vec<SocketAddr>>,
    resolved: Cell<bool>,
}

#[derive(Clone, Copy, Default)]
struct ServerStat {
    srtt_ms: u32,
    fails: u32,
    down_until: u64,
}

const ROOT_HINTS: &[(&str, &str)] = &[
    ("198.41.0.4", "2001:503:ba3e::2:30"),
    ("170.247.170.2", "2801:1b8:10::b"),
    ("192.33.4.12", "2001:500:2::c"),
    ("199.7.91.13", "2001:500:2d::d"),
    ("192.203.230.10", "2001:500:a8::e"),
    ("192.5.5.241", "2001:500:2f::f"),
    ("192.112.36.4", "2001:500:12::d0d"),
    ("198.97.190.53", "2001:500:1::53"),
    ("192.36.148.17", "2001:7fe::53"),
    ("192.58.128.30", "2001:503:c27::2:30"),
    ("193.0.14.129", "2001:7fd::1"),
    ("199.7.83.42", "2001:500:9f::42"),
    ("202.12.27.33", "2001:dc3::35"),
];

/// Names never sent to the internet: RFC 6761/7686 special-use names and
/// RFC 6303 locally-served reverse zones.
const LOCAL_NXDOMAIN: &[&str] = &[
    "onion", "invalid", "local", "home.arpa", "10.in-addr.arpa", "168.192.in-addr.arpa", "254.169.in-addr.arpa",
    "127.in-addr.arpa", "0.in-addr.arpa", "d.f.ip6.arpa", "8.e.f.ip6.arpa", "9.e.f.ip6.arpa", "a.e.f.ip6.arpa",
    "b.e.f.ip6.arpa",
    "16.172.in-addr.arpa", "17.172.in-addr.arpa", "18.172.in-addr.arpa", "19.172.in-addr.arpa", "20.172.in-addr.arpa",
    "21.172.in-addr.arpa", "22.172.in-addr.arpa", "23.172.in-addr.arpa", "24.172.in-addr.arpa", "25.172.in-addr.arpa",
    "26.172.in-addr.arpa", "27.172.in-addr.arpa", "28.172.in-addr.arpa", "29.172.in-addr.arpa", "30.172.in-addr.arpa",
    "31.172.in-addr.arpa",
];

pub struct Resolver {
    pub cfg: Config,
    cache: RefCell<HashMap<(Name, u16), (Rc<Answer>, u64, u64)>>,
    keys: RefCell<HashMap<Name, (KeyState, u64)>>,
    delegations: RefCell<HashMap<Name, (Rc<Delegation>, u64)>>,
    servers: RefCell<HashMap<IpAddr, ServerStat>>,
    /// None = not yet known; the first UDP attempt decides.
    udp_works: Cell<Option<bool>>,
    v4_down_until: Cell<u64>,
    v6_down_until: Cell<u64>,
    v4_streak: Cell<u32>,
    v6_streak: Cell<u32>,
    udp_silent: Cell<u32>,
    tcp_idle: RefCell<HashMap<String, Vec<(Conn, u64)>>>,
    pub http: Rc<tls::Pool>,
    pub stats: RefCell<Stats>,
    local_nx: Vec<Name>,
    root: Rc<Delegation>,
}

impl Resolver {
    pub fn new(cfg: Config, http: Rc<tls::Pool>) -> Rc<Resolver> {
        let mut addrs = Vec::new();
        for (v4, v6) in ROOT_HINTS {
            addrs.push(SocketAddr::new(v4.parse().unwrap(), 53));
            addrs.push(SocketAddr::new(v6.parse().unwrap(), 53));
        }
        let root = Rc::new(Delegation {
            zone: Name::root(),
            ns: Vec::new(),
            addrs: RefCell::new(addrs),
            resolved: Cell::new(true),
        });
        let udp = match cfg.transport {
            Transport::Udp => Some(true),
            Transport::Tcp => Some(false),
            // the platform's egress carries TCP only
            Transport::Auto if net::egress_proxy() => Some(false),
            Transport::Auto => None,
        };
        Rc::new(Resolver {
            cfg,
            cache: RefCell::default(),
            keys: RefCell::default(),
            delegations: RefCell::default(),
            servers: RefCell::default(),
            udp_works: Cell::new(udp),
            v4_down_until: Cell::new(0),
            v6_down_until: Cell::new(0),
            v4_streak: Cell::new(0),
            v6_streak: Cell::new(0),
            udp_silent: Cell::new(0),
            tcp_idle: RefCell::default(),
            http,
            stats: RefCell::default(),
            local_nx: LOCAL_NXDOMAIN.iter().map(|s| Name::parse(s).unwrap()).collect(),
            root,
        })
    }

    pub fn transport_text(&self) -> String {
        let t = match self.udp_works.get() {
            Some(true) => "udp (tcp on truncation)",
            Some(false) => "tcp",
            None => "udp, not yet tried",
        };
        if self.cfg.forward.is_empty() {
            format!("recursive from the root, {t}")
        } else {
            format!("forwarding to {}", self.cfg.forward.iter().map(|u| u.text()).collect::<Vec<_>>().join(", "))
        }
    }

    pub fn cache_len(&self) -> usize {
        self.cache.borrow().len()
    }

    pub fn flush(&self, name: Option<&Name>) -> usize {
        let mut c = self.cache.borrow_mut();
        let before = c.len();
        match name {
            Some(n) => c.retain(|(k, _), _| !k.is_subdomain_of(n)),
            None => c.clear(),
        }
        if name.is_none() {
            self.keys.borrow_mut().clear();
            self.delegations.borrow_mut().clear();
        }
        before - c.len()
    }

    // ------------------------------------------------------------ cache --

    fn cache_get(&self, name: &Name, qtype: u16) -> Option<Answer> {
        let now = net::now_ms();
        let c = self.cache.borrow();
        let (a, exp, at) = c.get(&(name.to_lowercase(), qtype))?;
        if now >= *exp {
            return None;
        }
        let age = ((now - at) / 1000) as u32;
        let mut a = (**a).clone();
        for r in a.answer.iter_mut().chain(a.authority.iter_mut()) {
            r.ttl = r.ttl.saturating_sub(age);
        }
        a.ttl = a.ttl.saturating_sub(age);
        a.source = "cache";
        Some(a)
    }

    fn cache_put(&self, name: &Name, qtype: u16, a: &Answer) {
        let ttl = match a.rcode {
            rcode::NOERROR | rcode::NXDOMAIN => a.ttl,
            _ => a.ttl.min(30),
        };
        if ttl == 0 {
            return;
        }
        let now = net::now_ms();
        let mut c = self.cache.borrow_mut();
        if c.len() >= self.cfg.cache_entries {
            c.retain(|_, (_, exp, _)| *exp > now);
            if c.len() >= self.cfg.cache_entries {
                let drop: Vec<_> = c.keys().take(self.cfg.cache_entries / 10 + 1).cloned().collect();
                for k in drop {
                    c.remove(&k);
                }
            }
        }
        c.insert((name.to_lowercase(), qtype), (Rc::new(a.clone()), now + ttl as u64 * 1000, now));
    }

    pub fn is_local_nx(&self, name: &Name) -> bool {
        self.local_nx.iter().any(|z| name.is_subdomain_of(z))
    }

    // ------------------------------------------------------------ lookup --

    /// The validated answer for exactly (name, qtype).
    pub fn lookup<'a>(self: &'a Rc<Self>, name: &'a Name, qtype: u16, ctx: &'a Rc<Ctx>, depth: u32) -> Fut<'a, Answer> {
        Box::pin(async move {
            if self.is_local_nx(name) {
                let mut a = Answer::negative(rcode::NXDOMAIN, 3600, "local");
                a.ede = Some((ede::BLOCKED, "special-use name, never sent to the internet (RFC 6761/7686)".into()));
                return a;
            }
            if let Some(a) = self.cache_get(name, qtype) {
                self.stats.borrow_mut().cache_hits += 1;
                return a;
            }
            self.stats.borrow_mut().cache_misses += 1;
            if depth > MAX_DEPTH {
                return Answer::fail(ede::OTHER, "resolution nested too deeply");
            }
            let (msg, zone) = match self.fetch(name, qtype, ctx, depth).await {
                Ok(x) => x,
                Err(a) => {
                    self.cache_put(name, qtype, &a);
                    return a;
                }
            };
            let mut a = extract(&msg, name, qtype, &zone);
            a.ttl = a.ttl.min(if a.answer.is_empty() { self.cfg.max_negative_ttl } else { self.cfg.max_ttl });
            if self.cfg.validate {
                self.validate(name, qtype, &mut a, &zone, ctx, depth).await;
            } else {
                a.sec = Sec::Insecure;
            }
            {
                let mut s = self.stats.borrow_mut();
                match a.sec {
                    Sec::Secure => s.secure += 1,
                    Sec::Bogus => s.bogus += 1,
                    _ => s.insecure += 1,
                }
            }
            if a.sec == Sec::Bogus {
                a.ttl = a.ttl.min(60);
            }
            // RFC 9824: a validated compact denial (NSEC at the name with
            // NXNAME) is a nonexistent name; say so plainly
            if a.sec == Sec::Secure
                && a.rcode == rcode::NOERROR
                && a.answer.is_empty()
                && a.authority.iter().any(|r| {
                    r.rtype == ty::NSEC && r.name == *name && Nsec::parse(&r.name, &r.data).is_some_and(|n| dnssec::bitmap_has(&n.bitmap, ty::NXNAME))
                })
            {
                a.rcode = rcode::NXDOMAIN;
            }
            self.cache_put(name, qtype, &a);
            a
        })
    }

    /// Get a response for (name, qtype) from the network, with the zone it
    /// is authoritative for (the bailiwick its records must stay inside).
    async fn fetch(self: &Rc<Self>, name: &Name, qtype: u16, ctx: &Rc<Ctx>, depth: u32) -> Result<(Message, Name), Answer> {
        if !self.cfg.forward.is_empty() {
            return self.forward(name, qtype, ctx).await.map(|m| (m, Name::root()));
        }
        self.iterate(name, qtype, ctx, depth).await
    }

    // --------------------------------------------------------- iterating --

    fn closest_delegation(&self, name: &Name) -> Rc<Delegation> {
        let now = net::now_ms();
        let d = self.delegations.borrow();
        let mut cur = Some(name.clone());
        while let Some(n) = cur {
            if n.is_root() {
                break;
            }
            if let Some((del, exp)) = d.get(&n.to_lowercase()) {
                if *exp > now {
                    return del.clone();
                }
            }
            cur = n.parent();
        }
        self.root.clone()
    }

    async fn iterate(self: &Rc<Self>, qname: &Name, qtype: u16, ctx: &Rc<Ctx>, depth: u32) -> Result<(Message, Name), Answer> {
        // DS lives at the parent side of a cut
        let target = if qtype == ty::DS && !qname.is_root() { qname.parent().unwrap() } else { qname.clone() };
        let mut del = self.closest_delegation(&target);
        let mut min_labels = del.zone.label_count() + 1;
        let mut minimise = self.cfg.qname_minimisation;
        for _ in 0..40 {
            let full = !minimise || min_labels >= qname.label_count() || qtype == ty::DS && min_labels >= target.label_count();
            let (sname, stype) = if full { (qname.clone(), qtype) } else { (qname.suffix(min_labels), ty::A) };
            let resp = self.ask(&del, &sname, stype, ctx, depth).await?;
            // a referral: NS for a zone strictly below this one, toward the target
            if resp.answers.is_empty() && !(resp.aa && resp.rcode == rcode::NOERROR && has_soa(&resp)) && resp.rcode == rcode::NOERROR {
                if let Some(child) = referral(&resp, &del.zone, &target) {
                    let ttl = resp.authority.iter().filter(|r| r.rtype == ty::NS).map(|r| r.ttl).min().unwrap_or(3600);
                    let ns: Vec<Name> = resp.authority.iter().filter(|r| r.rtype == ty::NS && r.name == child).filter_map(|r| r.target()).collect();
                    let mut glue = Vec::new();
                    for r in &resp.additional {
                        // glue only for servers named inside the referring zone
                        if !ns.contains(&r.name) || !r.name.is_subdomain_of(&del.zone) {
                            continue;
                        }
                        if let Some(ip) = rr_ip(r) {
                            glue.push(SocketAddr::new(ip, 53));
                        }
                    }
                    let nd = Rc::new(Delegation { zone: child.clone(), ns, resolved: Cell::new(false), addrs: RefCell::new(glue) });
                    self.delegations
                        .borrow_mut()
                        .insert(child.to_lowercase(), (nd.clone(), net::now_ms() + ttl.clamp(30, 172800) as u64 * 1000));
                    min_labels = child.label_count() + 1;
                    del = nd;
                    continue;
                }
                if !resp.aa && !full {
                    // not a referral, not authoritative: give up minimising
                    minimise = false;
                    continue;
                }
                if !resp.aa {
                    return Err(Answer::fail(ede::NOT_AUTHORITATIVE, format!("lame answer from {} servers", del.zone)));
                }
            }
            if !full {
                // RFC 9156: NXDOMAIN for an ancestor is trusted only after a full query
                if resp.rcode == rcode::NXDOMAIN {
                    minimise = false;
                    continue;
                }
                min_labels += 1;
                continue;
            }
            return Ok((resp, del.zone.clone()));
        }
        Err(Answer::fail(ede::OTHER, "too many referrals"))
    }

    /// Ask the zone's servers, best first, until one gives a usable answer.
    async fn ask(self: &Rc<Self>, del: &Rc<Delegation>, sname: &Name, stype: u16, ctx: &Rc<Ctx>, depth: u32) -> Result<Message, Answer> {
        if del.addrs.borrow().is_empty() && !del.resolved.get() {
            self.resolve_ns_addrs(del, ctx, depth).await;
        }
        let addrs = self.order(&del.addrs.borrow());
        if addrs.is_empty() {
            return Err(Answer::fail(ede::NO_REACHABLE_AUTHORITY, format!("no usable address for any {} server", del.zone)));
        }
        let mut last = Answer::fail(ede::NO_REACHABLE_AUTHORITY, format!("no {} server answered", del.zone));
        // up to 6 real attempts; a zone with one or two servers gets its lost
        // packets retried; a family found unreachable on the way is skipped
        let mut list = addrs.clone();
        while list.len() < 3 {
            list.extend(addrs.iter().copied());
        }
        let mut attempts = 0;
        for addr in list {
            let now = net::now_ms();
            let down = |v4: bool| if v4 { self.v4_down_until.get() > now } else { self.v6_down_until.get() > now };
            if down(addr.is_ipv4()) && addrs.iter().any(|a| !down(a.is_ipv4())) {
                continue;
            }
            attempts += 1;
            if attempts > 6 {
                break;
            }
            if !ctx.spend() {
                return Err(Answer::fail(ede::OTHER, "query budget for this request exhausted"));
            }
            match self.query_server(addr, sname, stype, false, ctx).await {
                Ok(m) => match m.rcode {
                    rcode::NOERROR | rcode::NXDOMAIN => return Ok(m),
                    rc => {
                        last = Answer::fail(ede::NETWORK_ERROR, format!("{addr} answered {}", rcode::name(rc)));
                    }
                },
                Err(e) => {
                    last = Answer::fail(ede::NETWORK_ERROR, format!("{addr}: {e}"));
                }
            }
        }
        Err(last)
    }

    async fn resolve_ns_addrs(self: &Rc<Self>, del: &Rc<Delegation>, ctx: &Rc<Ctx>, depth: u32) {
        del.resolved.set(true);
        let mut out = Vec::new();
        for ns in del.ns.iter().take(4) {
            // a server named inside the zone it serves needs glue we did not get
            if ns.is_subdomain_of(&del.zone) {
                continue;
            }
            for t in [ty::AAAA, ty::A] {
                if (t == ty::A && !self.cfg.ipv4) || (t == ty::AAAA && !self.cfg.ipv6) {
                    continue;
                }
                let a = self.lookup(ns, t, ctx, depth + 1).await;
                if a.sec == Sec::Bogus {
                    continue;
                }
                out.extend(a.answer.iter().filter(|r| r.name == *ns).filter_map(rr_ip).map(|ip| SocketAddr::new(ip, 53)));
            }
            if out.len() >= 4 {
                break;
            }
        }
        del.addrs.borrow_mut().extend(out);
    }

    /// Usable addresses, fastest first, families that work first.
    fn order(&self, addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        let now = net::now_ms();
        let st = self.servers.borrow();
        let mut v: Vec<(u64, SocketAddr)> = addrs
            .iter()
            .filter(|a| (a.is_ipv4() && self.cfg.ipv4) || (a.is_ipv6() && self.cfg.ipv6))
            .map(|a| {
                let s = st.get(&a.ip()).copied().unwrap_or_default();
                let fam_down = if a.is_ipv4() { self.v4_down_until.get() } else { self.v6_down_until.get() } > now;
                let mut score = if s.srtt_ms == 0 { 50 + (net::random_u16() % 50) as u64 } else { s.srtt_ms as u64 };
                if s.down_until > now {
                    score += 100_000;
                }
                if fam_down {
                    score += 50_000;
                }
                // TCP-only usually means the platform's egress, which dials IPv6
                if self.udp_works.get() == Some(false) && a.is_ipv4() {
                    score += 500;
                }
                (score, *a)
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v.into_iter().map(|x| x.1).collect()
    }

    fn note(&self, ip: IpAddr, ok: bool, rtt_ms: u64) {
        let (streak, down) = if ip.is_ipv4() { (&self.v4_streak, &self.v4_down_until) } else { (&self.v6_streak, &self.v6_down_until) };
        if ok {
            streak.set(0);
        } else {
            streak.set(streak.get() + 1);
            // a whole family failing in a row (e.g. an egress that only carries IPv6)
            if streak.get() >= 6 {
                down.set(net::now_ms() + 120_000);
                streak.set(0);
            }
        }
        let mut st = self.servers.borrow_mut();
        let s = st.entry(ip).or_default();
        if ok {
            s.srtt_ms = if s.srtt_ms == 0 { rtt_ms as u32 } else { (s.srtt_ms * 7 + rtt_ms as u32) / 8 };
            s.fails = 0;
            s.down_until = 0;
        } else {
            s.fails += 1;
            s.srtt_ms = (s.srtt_ms.max(200) * 2).min(5000);
            if s.fails >= 3 {
                s.down_until = net::now_ms() + (5_000u64 << s.fails.min(6));
            }
        }
    }

    fn family_failed(&self, a: &SocketAddr, e: &io::Error) {
        use io::ErrorKind as K;
        if matches!(e.kind(), K::HostUnreachable | K::NetworkUnreachable | K::PermissionDenied | K::AddrNotAvailable | K::InvalidInput) {
            let until = net::now_ms() + 300_000;
            if a.is_ipv4() {
                self.v4_down_until.set(until);
            } else {
                self.v6_down_until.set(until);
            }
        }
    }

    /// One query to one server: UDP (unless it is known not to work), TCP
    /// on truncation or when UDP is unavailable.
    async fn query_server(&self, addr: SocketAddr, sname: &Name, stype: u16, rd: bool, ctx: &Ctx) -> io::Result<Message> {
        let id = net::random_u16();
        let q = Message::query(id, sname, stype, rd, rd, true);
        let wire = q.to_bytes(65535);
        let srtt = self.servers.borrow().get(&addr.ip()).map(|s| s.srtt_ms).unwrap_or(0);
        let wait = Duration::from_millis(if srtt == 0 { 1200 } else { (srtt as u64 * 3).clamp(300, 2000) }).min(ctx.left());
        if wait.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request deadline"));
        }
        self.stats.borrow_mut().upstream_queries += 1;
        let t0 = net::now_ms();
        let mut use_tcp = self.udp_works.get() == Some(false);
        if !use_tcp {
            self.stats.borrow_mut().upstream_udp += 1;
            match self.udp_exchange(addr, &wire, id, wait).await {
                Ok(b) => {
                    if self.udp_works.get().is_none() {
                        self.udp_works.set(Some(true));
                    }
                    let m = check_response(&b, id, sname, stype)?;
                    self.note(addr.ip(), true, net::now_ms() - t0);
                    if !m.tc {
                        return Ok(m);
                    }
                    use_tcp = true;
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::TimedOut {
                        self.stats.borrow_mut().upstream_timeouts += 1;
                        self.note(addr.ip(), false, 0);
                        if self.udp_works.get().is_none() {
                            // UDP that never once came back is UDP that is filtered
                            self.udp_silent.set(self.udp_silent.get() + 1);
                            if self.udp_silent.get() >= 6 {
                                eprintln!("[enclavedns] no UDP answer from any authority; querying over TCP");
                                self.udp_works.set(Some(false));
                            }
                        }
                        return Err(e);
                    }
                    if e.kind() == io::ErrorKind::PermissionDenied && self.udp_works.get().is_none() {
                        // the platform's egress denies raw UDP: TCP from now on
                        eprintln!("[enclavedns] outbound UDP refused ({e}); querying authorities over TCP");
                        self.udp_works.set(Some(false));
                        use_tcp = true;
                    } else {
                        self.family_failed(&addr, &e);
                        self.note(addr.ip(), false, 0);
                        return Err(e);
                    }
                }
            }
        }
        if use_tcp {
            self.stats.borrow_mut().upstream_tcp += 1;
            let wait = Duration::from_millis(3000).min(ctx.left());
            let key = format!("tcp {addr}");
            let r = self.stream_exchange(&key, &Dest::Addr(addr), None, &wire, wait).await;
            return match r {
                Ok(b) => {
                    self.note(addr.ip(), true, net::now_ms() - t0);
                    check_response(&b, id, sname, stype)
                }
                Err(e) => {
                    self.family_failed(&addr, &e);
                    self.note(addr.ip(), false, 0);
                    Err(e)
                }
            };
        }
        unreachable!()
    }

    async fn udp_exchange(&self, addr: SocketAddr, wire: &[u8], id: u16, wait: Duration) -> io::Result<Vec<u8>> {
        let any: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        let sock = Udp::bind(any).await?;
        sock.send_to(wire, addr).await?;
        net::timeout(wait, async {
            loop {
                let (b, from) = sock.recv().await?;
                // anything not from the server, or for another id, is noise (or spoofing)
                if from == addr && b.len() >= 12 && u16::from_be_bytes([b[0], b[1]]) == id {
                    return Ok(b);
                }
            }
        })
        .await
    }

    /// DNS over a stream (TCP or TLS), RFC 7766 length framing, reusing an
    /// idle connection to the same server when one is warm.
    async fn stream_exchange(&self, key: &str, dest: &Dest, tls_name: Option<&str>, wire: &[u8], wait: Duration) -> io::Result<Vec<u8>> {
        let mut framed = (wire.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(wire);
        let idle = {
            let now = net::now_ms();
            let mut m = self.tcp_idle.borrow_mut();
            let v = m.entry(key.to_string()).or_default();
            let mut got = None;
            while let Some((c, t)) = v.pop() {
                if now.saturating_sub(t) < 8_000 {
                    got = Some(c);
                    break;
                }
            }
            got
        };
        async fn once(c: &mut Conn, framed: &[u8]) -> io::Result<Vec<u8>> {
            c.write_all(framed).await?;
            let mut buf = Vec::new();
            let len = c.read_exact(2, &mut buf).await?;
            let n = u16::from_be_bytes([len[0], len[1]]) as usize;
            c.read_exact(n, &mut buf).await
        }
        if let Some(mut c) = idle {
            if let Ok(b) = net::timeout(wait, once(&mut c, &framed)).await {
                self.tcp_idle.borrow_mut().entry(key.to_string()).or_default().push((c, net::now_ms()));
                return Ok(b);
            }
        }
        let mut c = Conn::open(dest, tls_name, wait).await?;
        let b = net::timeout(wait, once(&mut c, &framed)).await?;
        let now = net::now_ms();
        let mut m = self.tcp_idle.borrow_mut();
        // connections to servers not asked again would otherwise stay open forever
        if m.len() > 32 {
            m.retain(|_, v| {
                v.retain(|(_, t)| now.saturating_sub(*t) < 8_000);
                !v.is_empty()
            });
        }
        let v = m.entry(key.to_string()).or_default();
        if v.len() < 4 {
            v.push((c, now));
        }
        Ok(b)
    }

    // --------------------------------------------------------- forwarding --

    async fn forward(self: &Rc<Self>, name: &Name, qtype: u16, ctx: &Rc<Ctx>) -> Result<Message, Answer> {
        let mut last = Answer::fail(ede::NO_REACHABLE_AUTHORITY, "no upstream answered");
        for up in &self.cfg.forward {
            if !ctx.spend() {
                break;
            }
            let id = net::random_u16();
            // CD: we validate ourselves, and need the data a validating upstream would withhold
            let q = Message::query(id, name, qtype, true, true, true);
            let wire = q.to_bytes(65535);
            let wait = Duration::from_millis(3000).min(ctx.left());
            self.stats.borrow_mut().upstream_queries += 1;
            let r: io::Result<Vec<u8>> = match up {
                Upstream::Udp(a) => match self.udp_exchange(*a, &wire, id, wait).await {
                    Ok(b) if Message::parse(&b).is_ok_and(|m| m.tc) => {
                        self.stream_exchange(&format!("tcp {a}"), &Dest::Addr(*a), None, &wire, wait).await
                    }
                    r => r,
                },
                Upstream::Tcp(a) => self.stream_exchange(&format!("tcp {a}"), &Dest::Addr(*a), None, &wire, wait).await,
                Upstream::Tls(a, n) => self.stream_exchange(&format!("tls {a} {n}"), &Dest::Addr(*a), Some(n), &wire, wait).await,
                Upstream::Https(u) => self.doh_exchange(u, &wire, wait).await,
            };
            match r.and_then(|b| check_response(&b, id, name, qtype)) {
                Ok(m) if m.rcode == rcode::NOERROR || m.rcode == rcode::NXDOMAIN => return Ok(m),
                Ok(m) => last = Answer::fail(ede::NETWORK_ERROR, format!("{} answered {}", up.text(), rcode::name(m.rcode))),
                Err(e) => last = Answer::fail(ede::NETWORK_ERROR, format!("{}: {e}", up.text())),
            }
        }
        Err(last)
    }

    /// Where an outbound host can be dialled: an IP literal; by name through
    /// the egress front; the host's own lookup (inside an isolated partition
    /// the only one that names its egress forwarders); else, when allowed,
    /// this resolver.
    pub async fn dests_for(self: &Rc<Self>, host: &str, port: u16, own_resolver: bool) -> Vec<Dest> {
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            return vec![Dest::Addr(SocketAddr::new(ip, port))];
        }
        if net::egress_proxy() {
            return vec![Dest::Host(host.to_string(), port)];
        }
        let mut ips = net::system_lookup(host).await;
        // v6 first: the platform's egress carries IPv6
        ips.sort_by_key(|ip| ip.is_ipv4());
        if !ips.is_empty() || !own_resolver {
            return ips.into_iter().map(|ip| Dest::Addr(SocketAddr::new(ip, port))).collect();
        }
        let Ok(n) = Name::parse(host) else { return Vec::new() };
        let mut out = Vec::new();
        for t in [ty::AAAA, ty::A] {
            let ctx = Ctx::new(Duration::from_secs(8));
            let mut name = n.clone();
            for _ in 0..8 {
                let a = self.lookup(&name, t, &ctx, 0).await;
                if a.sec == Sec::Bogus {
                    break;
                }
                if let Some(c) = a.cname_target(&name) {
                    name = c;
                    continue;
                }
                out.extend(a.answer.iter().filter_map(rr_ip).map(|ip| Dest::Addr(SocketAddr::new(ip, port))));
                break;
            }
        }
        out
    }

    async fn doh_exchange(self: &Rc<Self>, url: &str, wire: &[u8], wait: Duration) -> io::Result<Vec<u8>> {
        let u = tls::parse_url(url).map_err(io::Error::other)?;
        // never recurse into ourselves to find the upstream that is supposed to answer for us
        let dests = self.dests_for(&u.host, u.port, false).await;
        if dests.is_empty() {
            return Err(io::Error::other(format!("cannot resolve the DoH upstream {} (use an IP literal, or a name the host resolves)", u.host)));
        }
        let r = self
            .http
            .request(&u, &dests, "POST", &[("content-type", "application/dns-message"), ("accept", "application/dns-message")], wire, wait)
            .await?;
        if r.status != 200 {
            return Err(io::Error::other(format!("DoH upstream answered HTTP {}", r.status)));
        }
        Ok(r.body)
    }

    // --------------------------------------------------------- validation --

    /// The keys of `zone`, proven from the root anchors down.
    fn zone_keys<'a>(self: &'a Rc<Self>, zone: &'a Name, ctx: &'a Rc<Ctx>, depth: u32) -> Fut<'a, KeyState> {
        Box::pin(async move {
            let now = net::now_ms();
            if let Some((k, exp)) = self.keys.borrow().get(&zone.to_lowercase()) {
                if *exp > now {
                    return k.clone();
                }
            }
            let (k, ttl) = self.zone_keys_uncached(zone, ctx, depth).await;
            let ttl = match k {
                KeyState::Bogus(..) => ttl.min(30),
                _ => ttl,
            };
            if ttl > 0 {
                self.keys.borrow_mut().insert(zone.to_lowercase(), (k.clone(), now + ttl as u64 * 1000));
            }
            k
        })
    }

    async fn zone_keys_uncached(self: &Rc<Self>, zone: &Name, ctx: &Rc<Ctx>, depth: u32) -> (KeyState, u32) {
        if depth > MAX_DEPTH {
            return (KeyState::Bogus(ede::DNSSEC_INDETERMINATE, "chain of trust too deep".into()), 0);
        }
        let (ds, ds_ttl) = if zone.is_root() {
            (dnssec::root_anchor_ds(), 86400)
        } else {
            let a = self.lookup(zone, ty::DS, ctx, depth + 1).await;
            match a.sec {
                Sec::Insecure => return (KeyState::Insecure, a.ttl),
                Sec::Bogus | Sec::Indeterminate => {
                    let (c, w) = a.ede.clone().unwrap_or((ede::DNSSEC_BOGUS, "DS lookup failed".into()));
                    return (KeyState::Bogus(c, format!("{zone} DS: {w}")), a.ttl);
                }
                Sec::Secure => {}
            }
            let ds: Vec<Vec<u8>> = a.answer.iter().filter(|r| r.rtype == ty::DS && r.name == *zone).map(|r| r.data.clone()).collect();
            if ds.is_empty() {
                return (
                    KeyState::Bogus(ede::DNSSEC_BOGUS, format!("{zone} signs its data but its parent proves it has no DS and is no delegation")),
                    a.ttl,
                );
            }
            (ds, a.ttl)
        };
        let usable: Vec<&Vec<u8>> =
            ds.iter().filter(|d| d.len() > 4 && dnssec::alg_supported(d[2]) && dnssec::digest_supported(d[3])).collect();
        if usable.is_empty() {
            // RFC 4035 §5.2: only unknown algorithms/digests = treat as unsigned
            return (KeyState::Insecure, ds_ttl);
        }
        // the DNSKEY RRset, from the zone itself
        let resp = match self.fetch(zone, ty::DNSKEY, ctx, depth + 1).await {
            Ok((m, _)) => m,
            Err(a) => {
                let w = a.ede.map(|e| e.1).unwrap_or_default();
                return (KeyState::Bogus(ede::DNSKEY_MISSING, format!("{zone} DNSKEY unreachable: {w}")), 5);
            }
        };
        let key_rrs: Vec<&Record> = resp.answers.iter().filter(|r| r.rtype == ty::DNSKEY && r.name == *zone).collect();
        let sigs: Vec<Rrsig> = resp
            .answers
            .iter()
            .filter(|r| r.rtype == ty::RRSIG && r.name == *zone && r.covered() == ty::DNSKEY)
            .filter_map(|r| Rrsig::parse(&r.data))
            .collect();
        let keys: Vec<Dnskey> = key_rrs.iter().filter_map(|r| Dnskey::parse(&r.data)).collect();
        if keys.is_empty() {
            return (KeyState::Bogus(ede::DNSKEY_MISSING, format!("{zone} has a DS but no DNSKEY")), 30);
        }
        let sep: Vec<Dnskey> = keys.iter().filter(|k| usable.iter().any(|d| dnssec::ds_matches(zone, k, d) == Some(true))).cloned().collect();
        if sep.is_empty() {
            return (KeyState::Bogus(ede::DNSKEY_MISSING, format!("no {zone} DNSKEY matches its DS")), 30);
        }
        let rdatas: Vec<Vec<u8>> = key_rrs.iter().map(|r| r.data.clone()).collect();
        let now = net::now_unix() as u32;
        match dnssec::verify_rrset(zone, ty::DNSKEY, 1, &rdatas, &sigs, &sep, zone, now) {
            Ok(v) => {
                let ttl = key_rrs.iter().map(|r| r.ttl).min().unwrap_or(0).min(v.ttl_cap).min(ds_ttl).min(self.cfg.max_ttl);
                (KeyState::Secure(Rc::new(keys)), ttl)
            }
            Err((c, w)) => (KeyState::Bogus(c, format!("{zone} DNSKEY: {w}")), 30),
        }
    }

    /// Is data under `zone_hint` (and down to `name`) in unsigned territory?
    /// Walks the DS chain from the deepest known zone to the name: an
    /// unsigned delegation anywhere on the way means insecure; reaching the
    /// name inside signed zones means the missing signatures are bogus.
    async fn unsigned_status(self: &Rc<Self>, owner: &Name, rtype: u16, zone_hint: &Name, ctx: &Rc<Ctx>, depth: u32) -> (Sec, Option<(u16, String)>) {
        // DS lives in the parent zone: an unsigned DS can only be explained
        // by an unsigned delegation ABOVE its owner (and walking to the owner
        // itself would ask for this very DS again)
        let name = &if rtype == ty::DS { owner.parent().unwrap_or_else(Name::root) } else { owner.clone() };
        let start = if name.is_subdomain_of(zone_hint) { zone_hint.clone() } else { Name::root() };
        match self.zone_keys(&start, ctx, depth + 1).await {
            KeyState::Insecure => return (Sec::Insecure, None),
            KeyState::Bogus(c, w) => return (Sec::Bogus, Some((c, w))),
            KeyState::Secure(_) => {}
        }
        let mut below: Vec<Name> = Vec::new();
        let mut cur = name.clone();
        while cur.label_count() > start.label_count() {
            below.push(cur.clone());
            cur = cur.parent().unwrap();
        }
        below.reverse();
        for a in below {
            let ds = self.lookup(&a, ty::DS, ctx, depth + 1).await;
            match ds.sec {
                Sec::Insecure => return (Sec::Insecure, None),
                Sec::Bogus | Sec::Indeterminate => {
                    return (Sec::Bogus, ds.ede.clone().or(Some((ede::DNSSEC_BOGUS, format!("DS for {a} failed")))))
                }
                Sec::Secure => {}
            }
        }
        (Sec::Bogus, Some((ede::RRSIGS_MISSING, format!("{owner} {} is in a signed zone but unsigned", ty::name(rtype)))))
    }

    async fn validate(self: &Rc<Self>, qname: &Name, qtype: u16, a: &mut Answer, zone_hint: &Name, ctx: &Rc<Ctx>, depth: u32) {
        let now = net::now_unix() as u32;
        let mut worst = Sec::Secure;
        let mut why: Option<(u16, String)> = None;
        fn downgrade(why: &mut Option<(u16, String)>, s: Sec, w: Option<(u16, String)>, worst: &mut Sec) {
            if s > *worst {
                *worst = s;
                if w.is_some() {
                    *why = w;
                }
            }
        }
        let sigs_of = |recs: &[Record], owner: &Name, t: u16| -> Vec<Rrsig> {
            recs.iter()
                .filter(|r| r.rtype == ty::RRSIG && r.name == *owner && r.covered() == t)
                .filter_map(|r| Rrsig::parse(&r.data))
                .collect()
        };
        // verify one RRset; returns the wildcard labels if expanded
        let mut wildcard_labels: Option<u8> = None;
        let mut ttl_cap = u32::MAX;
        let answer_sets: Vec<Vec<Record>> = rrsets(&a.answer).into_iter().filter(|s| s[0].rtype != ty::RRSIG).collect();
        let synth_cname = a.answer.iter().any(|r| r.rtype == ty::DNAME);
        for set in &answer_sets {
            let (owner, t) = (&set[0].name, set[0].rtype);
            // a CNAME synthesized from a validated DNAME carries no signature
            if t == ty::CNAME && synth_cname && sigs_of(&a.answer, owner, t).is_empty() {
                continue;
            }
            let (s, w, cap, wl) = self.verify_set(set, &sigs_of(&a.answer, owner, t), zone_hint, now, ctx, depth).await;
            ttl_cap = ttl_cap.min(cap);
            if wl.is_some() {
                wildcard_labels = wl;
            }
            downgrade(&mut why, s, w, &mut worst);
        }
        let positive = !answer_sets.is_empty();
        // authority: SOA and denial records, each RRset verified
        let auth_sets: Vec<Vec<Record>> = rrsets(&a.authority)
            .into_iter()
            .filter(|s| matches!(s[0].rtype, ty::NSEC | ty::NSEC3 | ty::SOA))
            .collect();
        let need_proof = !positive || wildcard_labels.is_some();
        if need_proof && worst == Sec::Secure {
            let mut nsecs = Vec::new();
            let mut nsec3s = Vec::new();
            let mut auth_sec = Sec::Secure;
            let mut signed_any = false;
            for set in &auth_sets {
                let (owner, t) = (&set[0].name, set[0].rtype);
                let sigs = sigs_of(&a.authority, owner, t);
                signed_any |= !sigs.is_empty();
                let (s, w, cap, _) = self.verify_set(set, &sigs, zone_hint, now, ctx, depth).await;
                ttl_cap = ttl_cap.min(cap);
                if s == Sec::Secure {
                    for r in set {
                        match t {
                            ty::NSEC => nsecs.extend(Nsec::parse(owner, &r.data)),
                            ty::NSEC3 => nsec3s.extend(Nsec3::parse(owner, &r.data)),
                            _ => {}
                        }
                    }
                }
                if s > auth_sec {
                    auth_sec = s;
                    if s != Sec::Secure {
                        why = w;
                    }
                }
            }
            if !signed_any && !positive {
                // nothing signed: secure only if the zone is provably unsigned
                let soa_owner = auth_sets.iter().find(|s| s[0].rtype == ty::SOA).map(|s| s[0].name.clone());
                let (s, w) = match soa_owner {
                    Some(z) => self.unsigned_status(&z, ty::SOA, zone_hint, ctx, depth).await,
                    None => self.unsigned_status(qname, qtype, zone_hint, ctx, depth).await,
                };
                downgrade(&mut why, s, w, &mut worst);
            } else if auth_sec == Sec::Insecure {
                downgrade(&mut why, Sec::Insecure, None, &mut worst);
            } else if auth_sec != Sec::Secure {
                { let w = why.clone(); downgrade(&mut why, auth_sec, w, &mut worst); }
            } else {
                let claim = match (wildcard_labels, a.rcode) {
                    (Some(l), _) => Claim::WildcardAnswer { labels: l },
                    (None, rcode::NXDOMAIN) => Claim::NxDomain,
                    _ => Claim::NoData,
                };
                match dnssec::prove(qname, qtype, claim, &nsecs, &nsec3s) {
                    Denial::Secure => {}
                    Denial::Insecure(c, w) => downgrade(&mut why, Sec::Insecure, Some((c, w)), &mut worst),
                    Denial::Bogus(c, w) => downgrade(&mut why, Sec::Bogus, Some((c, w)), &mut worst),
                }
            }
        }
        a.sec = worst;
        if worst == Sec::Bogus || worst == Sec::Indeterminate {
            a.ede = why.or(Some((ede::DNSSEC_BOGUS, "validation failed".into())));
        } else if let Some((c, w)) = why {
            if c == ede::UNSUPPORTED_NSEC3_ITERATIONS || c == ede::UNSUPPORTED_DNSKEY_ALG {
                a.ede = Some((c, w));
            }
        }
        if worst == Sec::Secure {
            a.ttl = a.ttl.min(ttl_cap);
        }
    }

    /// Verify one RRset: (status, reason, ttl cap, wildcard labels).
    async fn verify_set(
        self: &Rc<Self>,
        set: &[Record],
        sigs: &[Rrsig],
        zone_hint: &Name,
        now: u32,
        ctx: &Rc<Ctx>,
        depth: u32,
    ) -> (Sec, Option<(u16, String)>, u32, Option<u8>) {
        let (owner, t) = (&set[0].name, set[0].rtype);
        if sigs.is_empty() {
            let (s, w) = self.unsigned_status(owner, t, zone_hint, ctx, depth).await;
            return (s, w, u32::MAX, None);
        }
        // the signer must enclose the owner and sit inside the server's bailiwick
        let signer = sigs
            .iter()
            .map(|s| s.signer.clone())
            .find(|s| owner.is_subdomain_of(s) && s.is_subdomain_of(zone_hint))
            .or_else(|| sigs.iter().map(|s| s.signer.clone()).find(|s| owner.is_subdomain_of(s)));
        let Some(signer) = signer else {
            return (Sec::Bogus, Some((ede::DNSSEC_BOGUS, format!("{owner} signed by a zone that does not enclose it"))), u32::MAX, None);
        };
        if t == ty::DS && signer == *owner {
            return (Sec::Bogus, Some((ede::DNSSEC_BOGUS, format!("DS for {owner} signed by the child zone"))), u32::MAX, None);
        }
        match self.zone_keys(&signer, ctx, depth + 1).await {
            KeyState::Insecure => (Sec::Insecure, None, u32::MAX, None),
            KeyState::Bogus(c, w) => (Sec::Bogus, Some((c, w)), u32::MAX, None),
            KeyState::Secure(keys) => {
                let rdatas: Vec<Vec<u8>> = set.iter().map(|r| r.data.clone()).collect();
                match dnssec::verify_rrset(owner, t, set[0].class, &rdatas, sigs, &keys, &signer, now) {
                    Ok(v) => (Sec::Secure, None, v.ttl_cap, if v.wildcard { Some(v.labels) } else { None }),
                    Err(w) => (Sec::Bogus, Some(w), u32::MAX, None),
                }
            }
        }
    }
}

fn has_soa(m: &Message) -> bool {
    m.authority.iter().any(|r| r.rtype == ty::SOA)
}

pub fn rr_ip(r: &Record) -> Option<IpAddr> {
    match (r.rtype, r.data.len()) {
        (ty::A, 4) => Some(IpAddr::from(<[u8; 4]>::try_from(&r.data[..]).ok()?)),
        (ty::AAAA, 16) => Some(IpAddr::from(<[u8; 16]>::try_from(&r.data[..]).ok()?)),
        _ => None,
    }
}

/// The child zone a referral points at, if this response is one: NS
/// records for a name strictly below `zone`, at or above `target`.
fn referral(m: &Message, zone: &Name, target: &Name) -> Option<Name> {
    m.authority
        .iter()
        .filter(|r| r.rtype == ty::NS)
        .map(|r| r.name.clone())
        .filter(|n| n.is_subdomain_of(zone) && n != zone && target.is_subdomain_of(n))
        .max_by_key(|n| n.label_count())
}

fn check_response(b: &[u8], id: u16, sname: &Name, stype: u16) -> io::Result<Message> {
    let m = Message::parse(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let ok = m.id == id
        && m.qr
        && m.questions.len() == 1
        && m.questions[0].name == *sname
        && m.questions[0].qtype == stype;
    if !ok {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "response does not match the query"));
    }
    Ok(m)
}

/// The records a response holds for exactly (qname, qtype), inside the
/// server's bailiwick, before validation.
pub fn extract(m: &Message, qname: &Name, qtype: u16, zone: &Name) -> Answer {
    let inside = |r: &&Record| r.name.is_subdomain_of(zone);
    let with_sigs = |recs: &[Record], pick: &dyn Fn(&Record) -> bool| -> Vec<Record> {
        let base: Vec<Record> = recs.iter().filter(inside).filter(|r| r.rtype != ty::RRSIG && pick(r)).cloned().collect();
        let mut out = base.clone();
        for r in recs.iter().filter(inside).filter(|r| r.rtype == ty::RRSIG) {
            if base.iter().any(|b| b.name == r.name && b.rtype == r.covered()) {
                out.push(r.clone());
            }
        }
        out
    };
    let mut a = Answer {
        rcode: m.rcode,
        answer: Vec::new(),
        authority: Vec::new(),
        additional: Vec::new(),
        sec: Sec::Indeterminate,
        ede: None,
        ttl: 0,
        aa: m.aa,
        source: "network",
    };
    let direct = with_sigs(&m.answers, &|r| r.name == *qname && r.rtype == qtype);
    let proof = with_sigs(&m.authority, &|r| matches!(r.rtype, ty::NSEC | ty::NSEC3));
    if direct.iter().any(|r| r.rtype == qtype) {
        a.rcode = rcode::NOERROR;
        a.answer = direct;
        a.authority = proof; // wildcard expansions carry their proof here
    } else if qtype != ty::CNAME && m.answers.iter().any(|r| r.rtype == ty::CNAME && r.name == *qname) {
        a.rcode = rcode::NOERROR;
        a.answer = with_sigs(&m.answers, &|r| r.name == *qname && r.rtype == ty::CNAME);
        a.authority = proof;
    } else if let Some(d) = m
        .answers
        .iter()
        .filter(|r| r.rtype == ty::DNAME && qname.is_subdomain_of(&r.name) && *qname != r.name)
        .max_by_key(|r| r.name.label_count())
    {
        let owner = d.name.clone();
        a.rcode = rcode::NOERROR;
        a.answer = with_sigs(&m.answers, &|r| r.name == owner && r.rtype == ty::DNAME);
        if let Some(target) = d.target() {
            if let Ok(new) = qname.replace_suffix(&owner, &target) {
                a.answer.push(Record::new(qname.clone(), ty::CNAME, d.ttl, new.wire().to_vec()));
            }
        }
    } else {
        a.answer.clear();
        a.authority = with_sigs(&m.authority, &|r| {
            matches!(r.rtype, ty::NSEC | ty::NSEC3) || (r.rtype == ty::SOA && qname.is_subdomain_of(&r.name))
        });
        if a.rcode != rcode::NXDOMAIN {
            a.rcode = rcode::NOERROR;
        }
    }
    a.ttl = if !a.answer.is_empty() {
        a.answer.iter().filter(|r| r.rtype != ty::RRSIG).map(|r| r.ttl).min().unwrap_or(0)
    } else {
        a.authority
            .iter()
            .find(|r| r.rtype == ty::SOA)
            .map(|r| {
                let min = r.data.len().checked_sub(4).map(|i| u32::from_be_bytes(r.data[i..].try_into().unwrap())).unwrap_or(0);
                r.ttl.min(min)
            })
            .unwrap_or(60)
    };
    a
}
