//! The outbound libp2p engine — milestone 4-6. This stub keeps the app
//! honest about what it can do until then.
#![allow(dead_code)]

pub struct Dht {
    bootstrap: Vec<String>,
}

impl Dht {
    pub fn new(bootstrap: Vec<String>) -> Dht {
        eprintln!("[ipns-publisher] DHT publish requested, but the libp2p stack is not built yet (milestones 4-6); running http-only");
        Dht { bootstrap }
    }
    pub fn publish(&mut self, _routing_key: Vec<u8>, _record: Vec<u8>) {}
    pub fn drive(&mut self) -> bool { false }
    pub fn status_json(&self) -> String { "{\"state\":\"not-built\"}".into() }
    pub fn status_line(&self) -> String { "DHT client not built yet (records go out via delegates only)".into() }
}
