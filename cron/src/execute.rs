use crate::{
    config::Config,
    engine::{Action, Run},
    net::{Pending, Reply},
};
use serde_json::{json, Value};
pub fn start(cfg: &Config, run: &Run) -> Result<Pending, String> {
    let target = cfg.target(run.action.target(), &run.owner)?;
    let mut headers: Vec<_> = target
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    headers.extend([
        ("x-user".into(), run.owner.clone()),
        ("x-enclave-job".into(), run.job_id.clone()),
        ("x-enclave-run".into(), run.id.clone()),
        ("idempotency-key".into(), run.id.clone()),
    ]);
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("content-type".into(), "application/json".into()));
    }
    let body = match &run.action {
        Action::Eyesoff { prompt, .. } => {
            if target.kind != "eyesoff" {
                return Err("Action/target kind mismatch".into());
            }
            headers.push((
                "x-api-key".into(),
                target
                    .api_keys
                    .get(&run.owner)
                    .ok_or("No personal callback key")?
                    .clone(),
            ));
            // A bounded turn executes Eyesoff's own configured tools. The scheduler
            // never invents a user, injects a system role, or shares an admin key.
            serde_json::to_vec(&json!({"model":target.model,"messages":[{"role":"user","content":prompt}],"max_tokens":2048,"loop":{"persist":false,"max_calls":8,"max_seconds":target.timeout_s.saturating_sub(15).max(1)}})).unwrap()
        }
        Action::Http { body, .. } => {
            if target.kind != "http" {
                return Err("Action/target kind mismatch".into());
            }
            if target.method == "GET" {
                Vec::new()
            } else {
                serde_json::to_vec(body).map_err(|_| "Invalid callback body")?
            }
        }
    };
    Pending::start(
        &target.method,
        &target.url,
        &headers,
        &body,
        target.timeout_s,
        512 * 1024,
    )
}
pub fn result(action: &Action, r: Reply) -> (String, String, Option<u16>) {
    if !(200..300).contains(&r.status) {
        return (
            "failed".into(),
            format!("Callback returned HTTP {}", r.status),
            Some(r.status),
        );
    }
    if matches!(action, Action::Eyesoff { .. }) {
        let text = String::from_utf8_lossy(&r.body);
        let mut answer = String::new();
        let mut done = false;
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v.get("error").is_some() {
                        return ("failed".into(),"Eyesoff reported a generation/tool error; inspect the app without exposing callback credentials".into(),Some(r.status));
                    }
                    if v.get("tool").is_some() {
                        answer.clear();
                    }
                    if let Some(d) = v.get("delta").and_then(Value::as_str) {
                        answer.push_str(d);
                    }
                    if v.get("done") == Some(&Value::Bool(true)) {
                        done = true;
                    }
                }
            }
        }
        if !done {
            return (
                "failed".into(),
                "Eyesoff stream ended without a completion event; outcome may be partial".into(),
                Some(r.status),
            );
        }
        ("succeeded".into(), answer, Some(r.status))
    } else {
        (
            "succeeded".into(),
            String::from_utf8_lossy(&r.body).into_owned(),
            Some(r.status),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn require_done() {
        let a = Action::Eyesoff {
            target: "ai".into(),
            prompt: "hello".into(),
        };
        let r = |b: &str| Reply {
            status: 200,
            headers: vec![],
            body: b.as_bytes().to_vec(),
        };
        assert_eq!(result(&a, r("data: {\"delta\":\"hello\"}\n\n")).0, "failed");
        assert_eq!(
            result(
                &a,
                r("data: {\"delta\":\"hello\"}\n\ndata: {\"done\":true}\n\n")
            )
            .1,
            "hello"
        );
        assert_eq!(
            result(&a, r("data: {\"error\":\"secret\"}\n\n")).0,
            "failed"
        );
    }
}
