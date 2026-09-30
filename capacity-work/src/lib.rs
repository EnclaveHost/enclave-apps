use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub seed: String,
    pub rounds: u32,
    pub memory_mib: u32,
    pub passes: u32,
}
#[derive(Deserialize, Serialize, Debug, PartialEq)]
pub struct ResultData {
    pub digest: String,
    pub rounds: u32,
    pub memory_bytes: u64,
    pub passes: u32,
}
pub fn run(w: &Work) -> Result<ResultData, String> {
    if w.seed.len() != 64 || !w.seed.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("seed must be 32 bytes of hex".into());
    }
    if w.rounds == 0 || w.rounds > 2_000_000 || w.memory_mib > 2048 || w.passes == 0 || w.passes > 8
    {
        return Err("work outside bounds".into());
    }
    let seed: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&w.seed[i..i + 2], 16).unwrap())
        .collect();
    let mut state: [u8; 32] = seed.try_into().unwrap();
    for i in 0..w.rounds {
        let mut h = Sha256::new();
        h.update(state);
        h.update(i.to_le_bytes());
        state = h.finalize().into();
    }
    let n = (w.memory_mib as usize) * 1024 * 1024 / 32;
    let mut memory = Vec::<[u8; 32]>::new();
    memory
        .try_reserve_exact(n)
        .map_err(|_| "allocation unavailable")?;
    for i in 0..n {
        let mut h = Sha256::new();
        h.update(state);
        h.update((i as u64).to_le_bytes());
        state = h.finalize().into();
        memory.push(state);
    }
    for pass in 0..w.passes {
        for i in 0..n {
            let other = (u64::from_le_bytes(state[..8].try_into().unwrap()) % (n as u64)) as usize;
            let mut h = Sha256::new();
            h.update(memory[i]);
            h.update(memory[other]);
            h.update(state);
            h.update(pass.to_le_bytes());
            state = h.finalize().into();
            memory[i] = state;
        }
    }
    // Final response commits to all populated memory, not a claimed byte count.
    let mut h = Sha256::new();
    h.update(state);
    for block in &memory {
        h.update(block);
    }
    Ok(ResultData {
        digest: format!("{:x}", h.finalize()),
        rounds: w.rounds,
        memory_bytes: (n as u64) * 32,
        passes: w.passes,
    })
}
#[cfg(target_arch = "wasm32")]
mod http {
    use super::*;
    use wasip2::http::types::{
        Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
    };
    pub struct Component;
    fn authenticated(request: &IncomingRequest) -> bool {
        let Ok(token) = std::env::var("CAPACITY_WORK_TOKEN") else {
            return false;
        };
        if token.len() < 32 {
            return false;
        }
        let values = request.headers().get("authorization");
        if values.len() != 1 {
            return false;
        }
        let expected = Sha256::digest(format!("Bearer {token}").as_bytes());
        let got = Sha256::digest(&values[0]);
        expected
            .iter()
            .zip(got.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
    fn respond(out: ResponseOutparam, status: u16, bytes: &[u8]) {
        let headers = Fields::new();
        let _ = headers.set("content-type", &[b"application/json".to_vec()]);
        let response = OutgoingResponse::new(headers);
        let _ = response.set_status_code(status);
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let stream = body.write().unwrap();
        let _ = stream.blocking_write_and_flush(bytes);
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    }
    impl wasip2::exports::http::incoming_handler::Guest for Component {
        fn handle(request: IncomingRequest, out: ResponseOutparam) {
            if !authenticated(&request) {
                respond(out, 401, b"{\"error\":\"authorization required\"}");
                return;
            }
            if !matches!(request.method(), Method::Post)
                || request.path_with_query().as_deref() != Some("/v1/run")
            {
                respond(out, 404, b"{\"error\":\"POST /v1/run required\"}");
                return;
            }
            let Ok(body) = request.consume() else {
                respond(out, 400, b"{}");
                return;
            };
            let Ok(stream) = body.stream() else {
                respond(out, 400, b"{}");
                return;
            };
            let mut bytes = Vec::new();
            loop {
                match stream.blocking_read(4096) {
                    Ok(b) => {
                        bytes.extend(b);
                        if bytes.len() > 4096 {
                            respond(out, 413, b"{}");
                            return;
                        }
                    }
                    Err(wasip2::io::streams::StreamError::Closed) => break,
                    Err(_) => {
                        respond(out, 400, b"{}");
                        return;
                    }
                }
            }
            let result = serde_json::from_slice::<Work>(&bytes)
                .map_err(|_| "invalid request".to_string())
                .and_then(|w| run(&w));
            match result {
                Ok(r) => respond(out, 200, &serde_json::to_vec(&r).unwrap()),
                Err(e) => respond(
                    out,
                    422,
                    &serde_json::to_vec(&serde_json::json!({"error":e})).unwrap(),
                ),
            }
        }
    }
}
#[cfg(target_arch = "wasm32")]
use http::Component;
#[cfg(target_arch = "wasm32")]
wasip2::http::proxy::export!(Component);
#[cfg(test)]
mod tests {
    use super::*;
    fn work() -> Work {
        Work {
            seed: "01".repeat(32),
            rounds: 100,
            memory_mib: 1,
            passes: 1,
        }
    }
    #[test]
    fn deterministic_but_fresh_seed_changes_result() {
        let w = work();
        assert_eq!(run(&w).unwrap(), run(&w).unwrap());
        let mut other = w.clone();
        other.seed = "02".repeat(32);
        assert_ne!(run(&w).unwrap(), run(&other).unwrap());
    }
    #[test]
    fn bounds_and_shape() {
        let mut w = work();
        w.memory_mib = 2049;
        assert!(run(&w).is_err());
        w = work();
        w.rounds = 0;
        assert!(run(&w).is_err());
        w = work();
        w.seed = "x".repeat(64);
        assert!(run(&w).is_err());
        assert!(serde_json::from_str::<Work>("{\"audit\":true}").is_err());
    }
    #[test]
    fn memory_and_passes_change_commitment() {
        let w = work();
        let mut other = w.clone();
        other.passes = 2;
        assert_ne!(run(&w).unwrap(), run(&other).unwrap());
        other = w.clone();
        other.memory_mib = 0;
        assert_ne!(run(&w).unwrap(), run(&other).unwrap());
    }
}
