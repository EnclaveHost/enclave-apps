//! The app config (`ENCLAVE_CONFIG`, JSON) and the platform's port map.
//! Values written `$NAME` are deployment secrets: the platform substitutes
//! them before start, and one left unsubstituted is read from the guest env.

use crate::auth::ZoneConfig;
use crate::ens::EnsConfig;
use crate::onion::OnionConfig;
use crate::resolver::{self, Transport, Upstream};
use serde_json::Value;
use std::collections::HashMap;
use std::net::IpAddr;

pub struct Config {
    pub resolver: resolver::Config,
    pub recursion: bool,
    pub zones: Vec<ZoneConfig>,
    pub zone_seed: Option<Vec<u8>>,
    pub ens: Option<EnsConfig>,
    pub ens_alias: bool,
    pub onion: Option<OnionConfig>,
    pub udp_rate: f64,
    pub udp_burst: f64,
    pub admin_token: Option<String>,
    pub bind: IpAddr,
    pub ports: Ports,
}

/// Actual ports to bind (the platform remaps every declared logical port).
#[derive(Clone, Debug)]
pub struct Ports {
    pub dns_udp: Option<u16>,
    pub dns_tcp: Option<u16>,
    pub dot: Option<u16>,
    pub http: Option<u16>,
}

fn secret(v: &str) -> Option<String> {
    match v.strip_prefix('$') {
        Some(name) => std::env::var(name).ok().filter(|s| !s.is_empty()),
        None => Some(v.to_string()),
    }
}

/// `udp:53=40123,tcp:53=40124,...` -> logical -> actual.
pub fn parse_ports(env: &str) -> HashMap<String, u16> {
    env.split(',')
        .filter_map(|e| {
            let (k, v) = e.trim().split_once('=')?;
            Some((k.to_string(), v.parse().ok()?))
        })
        .collect()
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let text = if let Ok(f) = std::env::var("ENCLAVE_CONFIG_FILE") {
            std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?
        } else if let Ok(s) = std::env::var("ENCLAVE_CONFIG") {
            s
        } else if let Ok(f) = std::env::var("ENCLAVEDNS_CONFIG") {
            std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?
        } else {
            "{}".into()
        };
        let text = if text.trim().is_empty() { "{}".to_string() } else { text };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("config is not JSON: {e}"))?;
        Config::from_json(&v, std::env::var("ENCLAVE_PORTS").ok().as_deref())
    }

    pub fn from_json(v: &Value, ports_env: Option<&str>) -> Result<Config, String> {
        let get_b = |o: &Value, k: &str, d: bool| o.get(k).and_then(|x| x.as_bool()).unwrap_or(d);
        let get_u = |o: &Value, k: &str, d: u64| o.get(k).and_then(|x| x.as_u64()).unwrap_or(d);
        let strs = |o: &Value, k: &str| -> Vec<String> {
            o.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
        };

        let r = v.get("resolver").cloned().unwrap_or(Value::Null);
        let mut rc = resolver::Config::default();
        rc.forward = strs(&r, "forward").iter().map(|s| Upstream::parse(s)).collect::<Result<_, _>>()?;
        rc.validate = get_b(&r, "validate", true);
        rc.qname_minimisation = get_b(&r, "qname_minimisation", true);
        rc.ipv4 = get_b(&r, "ipv4", true);
        rc.ipv6 = get_b(&r, "ipv6", true);
        rc.max_ttl = get_u(&r, "max_ttl", 86400) as u32;
        rc.max_negative_ttl = get_u(&r, "max_negative_ttl", 3600) as u32;
        rc.cache_entries = get_u(&r, "cache_entries", 50_000).clamp(100, 2_000_000) as usize;
        rc.transport = match r.get("transport").and_then(|x| x.as_str()).unwrap_or("auto") {
            "auto" => Transport::Auto,
            "udp" => Transport::Udp,
            "tcp" => Transport::Tcp,
            t => return Err(format!("resolver.transport {t:?}: auto, udp or tcp")),
        };

        let mut zones = Vec::new();
        for z in v.get("zones").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            let name = z.get("name").and_then(|x| x.as_str()).ok_or("every zone needs a name")?.to_string();
            zones.push(ZoneConfig {
                name,
                records: strs(&z, "records"),
                ttl: get_u(&z, "ttl", 3600) as u32,
                dnssec: get_b(&z, "dnssec", true),
            });
        }
        let zone_seed = v
            .get("zone_key_seed")
            .and_then(|x| x.as_str())
            .and_then(secret)
            .or_else(|| std::env::var("ENCLAVEDNS_ZONE_SEED").ok().filter(|s| !s.is_empty()));
        if let Some(s) = &zone_seed {
            if s.len() < 32 {
                return Err("zone_key_seed must be at least 32 characters".into());
            }
        }

        let e = v.get("ens").cloned().unwrap_or(Value::Null);
        let ens = if get_b(&e, "enabled", true) {
            let mut c = EnsConfig::default();
            let rpc = strs(&e, "rpc");
            if !rpc.is_empty() {
                c.rpc = rpc.iter().filter_map(|s| secret(s)).collect();
            }
            for u in &c.rpc {
                crate::tls::parse_url(u)?;
            }
            c.quorum = get_u(&e, "quorum", c.quorum.min(c.rpc.len()) as u64) as usize;
            if c.quorum == 0 || c.quorum > c.rpc.len() {
                return Err(format!("ens.quorum must be 1..={} (the number of RPCs)", c.rpc.len()));
            }
            c.ttl = get_u(&e, "ttl", 300) as u32;
            c.gateway = e.get("gateway").and_then(|x| x.as_str()).map(|s| s.trim_matches('.').to_string());
            Some(c)
        } else {
            None
        };
        let ens_alias = get_b(&e, "alias_ens_tld", true);

        let o = v.get("onion").cloned().unwrap_or(Value::Null);
        let onion = if get_b(&o, "enabled", true) {
            let parse_ips = |k: &str| -> Result<Vec<IpAddr>, String> {
                strs(&o, k).iter().map(|s| s.parse::<IpAddr>().map_err(|_| format!("onion.{k}: bad address {s}"))).collect()
            };
            Some(OnionConfig {
                a: parse_ips("a")?.into_iter().filter_map(|i| if let IpAddr::V4(x) = i { Some(x) } else { None }).collect(),
                aaaa: parse_ips("aaaa")?.into_iter().filter_map(|i| if let IpAddr::V6(x) = i { Some(x) } else { None }).collect(),
                cname_suffix: o.get("cname_suffix").and_then(|x| x.as_str()).map(|s| s.trim_matches('.').to_string()),
                ttl: get_u(&o, "ttl", 300) as u32,
            })
        } else {
            None
        };

        let rl = v.get("rate_limit").cloned().unwrap_or(Value::Null);
        let admin_token = v.get("admin_token").and_then(|x| x.as_str()).and_then(secret).or_else(|| std::env::var("ENCLAVEDNS_ADMIN_TOKEN").ok());
        if admin_token.as_ref().is_some_and(|t| t.len() < 16) {
            return Err("admin_token must be at least 16 characters".into());
        }

        // ports: the platform's map, else local defaults (or "listen" overrides)
        let pm = ports_env.map(parse_ports).unwrap_or_default();
        let l = v.get("listen").cloned().unwrap_or(Value::Null);
        let local = |k: &str, d: u16| -> Option<u16> {
            match l.get(k) {
                Some(Value::Null) => None,
                Some(x) => x.as_u64().map(|p| p as u16),
                None => Some(d),
            }
        };
        let ports = if ports_env.is_some() {
            Ports {
                dns_udp: pm.get("udp:53").copied(),
                dns_tcp: pm.get("tcp:53").copied(),
                dot: pm.get("tcp:853").copied(),
                http: pm.iter().find(|(k, _)| k.starts_with("http")).map(|(_, v)| *v),
            }
        } else {
            Ports { dns_udp: local("dns", 5300), dns_tcp: local("dns", 5300), dot: local("dot", 8530), http: local("http", 8000) }
        };
        let bind = l.get("bind").and_then(|x| x.as_str()).unwrap_or("127.0.0.1").parse().map_err(|_| "listen.bind: bad address")?;

        Ok(Config {
            resolver: rc,
            recursion: get_b(v, "recursion", true),
            zones,
            zone_seed: zone_seed.map(|s| s.into_bytes()),
            ens,
            ens_alias,
            onion,
            udp_rate: rl.get("udp_per_second").and_then(|x| x.as_f64()).unwrap_or(40.0),
            udp_burst: rl.get("udp_burst").and_then(|x| x.as_f64()).unwrap_or(80.0),
            admin_token,
            bind,
            ports,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_ports() {
        let c = Config::from_json(&serde_json::json!({}), Some("udp:53=41000,tcp:53=41001,tcp:853=41002,http:8000=41003")).unwrap();
        assert_eq!(c.ports.dns_udp, Some(41000));
        assert_eq!(c.ports.dot, Some(41002));
        assert_eq!(c.ports.http, Some(41003));
        assert!(c.recursion && c.ens.is_some() && c.onion.is_some());
        assert_eq!(c.ens.unwrap().quorum, 2);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::from_json(&serde_json::json!({"resolver": {"forward": ["8.8.8.8"]}}), None).is_err());
        assert!(Config::from_json(&serde_json::json!({"ens": {"rpc": ["https://a.example"], "quorum": 2}}), None).is_err());
        assert!(Config::from_json(&serde_json::json!({"zone_key_seed": "short"}), None).is_err());
        let ok = Config::from_json(&serde_json::json!({"resolver": {"forward": ["tls://1.1.1.1#cloudflare-dns.com", "udp://9.9.9.9"]}}), None).unwrap();
        assert_eq!(ok.resolver.forward.len(), 2);
    }
}
