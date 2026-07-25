//! nn-demo: proves (and now DEBUGS) the wasi-nn path end to end.
//!
//! Routes:
//!   /ping      - answers immediately, touches no wasi-nn: proves requests
//!                reach the app at all.
//!   /progress  - dumps /data/nn-progress.log: every inference request writes
//!                a stage marker BEFORE each wasi-nn call, so when a call
//!                hangs, the last line names the exact stage it died in -
//!                readable from outside even while the hung request never
//!                returns. ?clear=1 truncates the log.
//!   /          - runs the baked-in 110-byte ONNX graph (Y = X + B).
//!                ?target=cpu|gpu pins the execution target (strict, no
//!                fallback); default tries gpu and reports a cpu fallback.
//!
//! Stage markers also go to stderr (the tenant log the platform keeps for the
//! deployment owner). /data may be absent (storage_mb 0): markers then exist
//! only on stderr and /progress says so.
#[allow(warnings)]
mod bindings;

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::nn::graph::{load, ExecutionTarget, GraphEncoding};
use bindings::wasi::nn::tensor::{Tensor, TensorType};

static MODEL: &[u8] = include_bytes!("model.onnx");
const INPUT: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
const EXPECTED: [f32; 4] = [11.0, 22.0, 33.0, 44.0];
const PROGRESS: &str = "/data/nn-progress.log";

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Stage marker: stderr always; /data best-effort. Flushed BEFORE returning so
/// a hang in the NEXT call can't lose it.
fn mark(rid: u128, stage: &str) {
    let line = format!("[{} r{}] {}\n", now_ms(), rid, stage);
    eprint!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(PROGRESS) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

fn nn_err(stage: &str, e: bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

fn run(rid: u128, target: ExecutionTarget) -> Result<(Vec<f32>, Vec<(String, u128)>), String> {
    let tname = if matches!(target, ExecutionTarget::Gpu) { "gpu" } else { "cpu" };
    let mut timings = Vec::new();
    let mut step = |stage: &str, t0: u128| {
        let dt = now_ms() - t0;
        timings.push((stage.to_string(), dt));
    };

    mark(rid, &format!("load({tname}) begin"));
    let t = now_ms();
    let graph = load(&[MODEL.to_vec()], GraphEncoding::Onnx, target)
        .map_err(|e| { mark(rid, &format!("load({tname}) FAILED")); nn_err("load", e) })?;
    step("load", t);
    mark(rid, &format!("load({tname}) ok"));

    mark(rid, "init-execution-context begin");
    let t = now_ms();
    let ctx = graph
        .init_execution_context()
        .map_err(|e| { mark(rid, "init-execution-context FAILED"); nn_err("init", e) })?;
    step("init", t);
    mark(rid, "init-execution-context ok");

    let bytes: Vec<u8> = INPUT.iter().flat_map(|f| f.to_le_bytes()).collect();
    mark(rid, "tensor-new begin");
    let x = Tensor::new(&[1, 4], TensorType::Fp32, &bytes);
    mark(rid, "tensor-new ok");

    mark(rid, "compute begin");
    let t = now_ms();
    let outputs = ctx
        .compute(vec![("X".to_string(), x)])
        .map_err(|e| { mark(rid, "compute FAILED"); nn_err("compute", e) })?;
    step("compute", t);
    mark(rid, "compute ok");

    let (_, y) = outputs
        .into_iter()
        .next()
        .ok_or("compute returned no outputs")?;
    let data = y.data();
    mark(rid, "output read ok");
    Ok((
        data.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        timings,
    ))
}

fn json_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect(),
            '\n' => "\\n".chars().collect(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

fn respond(out: ResponseOutparam, body_json: String) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[b"application/json".to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    // chunk writes: the platform caps a single body write at 4096 bytes
    for chunk in body_json.as_bytes().chunks(4000) {
        if stream.blocking_write_and_flush(chunk).is_err() {
            break;
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let pq = req.path_with_query().unwrap_or_default();
        let (path, query) = pq.split_once('?').unwrap_or((pq.as_str(), ""));
        let rid = now_ms() % 100_000;

        if path == "/ping" {
            return respond(out, format!("{{\"ok\":true,\"pong\":true,\"t\":{}}}", now_ms()));
        }
        if path == "/progress" {
            if query.split('&').any(|kv| kv == "clear=1") {
                let _ = std::fs::remove_file(PROGRESS);
                return respond(out, "{\"ok\":true,\"cleared\":true}".into());
            }
            return match std::fs::read_to_string(PROGRESS) {
                Ok(s) => respond(out, format!("{{\"ok\":true,\"progress\":\"{}\"}}", json_escape(&s))),
                Err(e) => respond(out, format!(
                    "{{\"ok\":false,\"error\":\"no progress log ({}) - /data absent (storage_mb 0?) or nothing ran yet\"}}",
                    json_escape(&e.to_string()))),
            };
        }

        let forced = query.split('&').find_map(|kv| kv.strip_prefix("target=").map(str::to_string));
        mark(rid, &format!("request begin path={path} target={}", forced.as_deref().unwrap_or("auto")));
        // default: try gpu, report a cpu fallback; ?target= pins it (no fallback)
        let (target, result) = match forced.as_deref() {
            Some("cpu") => ("cpu".to_string(), run(rid, ExecutionTarget::Cpu)),
            Some("gpu") => ("gpu".to_string(), run(rid, ExecutionTarget::Gpu)),
            _ => match run(rid, ExecutionTarget::Gpu) {
                Ok(r) => ("gpu".to_string(), Ok(r)),
                Err(gpu_err) => (
                    format!("cpu (gpu failed: {gpu_err})"),
                    run(rid, ExecutionTarget::Cpu),
                ),
            },
        };
        let body_json = match result {
            Ok((vals, timings)) => {
                let ok = vals.len() == EXPECTED.len()
                    && vals.iter().zip(EXPECTED).all(|(a, b)| (a - b).abs() < 1e-4);
                mark(rid, "request done");
                let tj: Vec<String> = timings.iter().map(|(s, ms)| format!("\"{s}\":{ms}")).collect();
                format!(
                    "{{\"ok\":{ok},\"target\":\"{}\",\"output\":{:?},\"expected\":{:?},\"ms\":{{{}}}}}",
                    json_escape(&target), vals, EXPECTED, tj.join(",")
                )
            }
            Err(e) => {
                mark(rid, "request done (error)");
                format!(
                    "{{\"ok\":false,\"target\":\"{}\",\"error\":\"{}\"}}",
                    json_escape(&target), json_escape(&e)
                )
            }
        };
        respond(out, body_json);
    }
}

bindings::export!(Component with_types_in bindings);
