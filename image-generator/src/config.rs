//! App configuration: the model catalog, generation limits and the optional
//! API key. Defaults come from the embedded assets/app-config.json; a
//! deployment can override any field through the ENCLAVE_CONFIG env var - a
//! JSON object the platform passes from the deployment's on-chain configCid
//! (CID-verified by the enclave before it reaches us). Publish the app once,
//! point deployments at other SD-family volumes via config alone.
//!
//! MULTI-MODEL: the `models` catalog is a MAP keyed by VOLUME NAME -
//! `{ "<volume-name>": { <field overrides incl. display `name`> }, ... }`.
//! The key is the volume the platform mounts at /models/<key>; the entry's
//! `name` is what the UI shows and what a request's `model` selects. Each
//! entry's effective AppConfig = the top-level template with the entry's
//! fields overlaid, `model_volume` pinned to the key, and `name` taken from
//! the entry (defaulting to the key). This is the whole volume->model
//! mapping, explicit: a volume wrapped as `qwen-image-2512-sd` is served as
//! `qwen-image-2512` because the config says so - no name matching.
//!
//! ENCLAVE_CONFIG's `models` merges INTO the embedded catalog per volume key
//! (and per field within an entry), so a deployment can add one model - e.g.
//! {"models":{"flux2-klein-sd":{"name":"flux2-klein","default_steps":4}}} -
//! without restating the rest. No `models` key at all = a single model, the
//! top-level `name`/`model_volume`.
//!
//! Every model serves through the host's stable-diffusion.cpp wasi-nn
//! backend (the node preloads each MODEL_VOLUMES_SD volume at startup; the
//! guest load_by_name()s it by its volume name).

use serde::Deserialize;
use serde_json::Value;

pub static APP_CONFIG_JSON: &[u8] = include_bytes!("../assets/app-config.json");

/// Model volumes mount here (read-only), one dir per volume.
pub const MODELS_ROOT: &str = "/models";

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    /// display name: shown in the UI, echoed in /v1 responses, and matched by
    /// a request's `model`. Distinct from `model_volume` - the mount name.
    pub name: String,
    /// the attached model volume (Tinfoil Modelwrap) holding the weights: the
    /// platform mounts it read-only at /models/<model_volume> and preloads it
    /// through the sdcpp backend. In a `models` catalog this is the map KEY.
    pub model_volume: String,
    pub default_steps: usize,
    pub max_steps: usize,
    /// image side defaults/limits, pixels; sd.cpp wants multiples of 64
    pub default_size: u32,
    pub min_size: u32,
    pub max_size: u32,
    /// "gpu" in production; local dev boxes override to "cpu" via
    /// ENCLAVE_CONFIG. No silent fallback: image gen on CPU is minutes, not
    /// seconds - failing loudly beats surprising the payer.
    pub default_target: String,
    /// cap on n for /v1/images/generations
    pub max_images: usize,
    /// classifier-free-guidance scale (1.0 = off, the turbo/lightning
    /// distilled setting; ~4 for undistilled models). Per-request "cfg"
    /// overrides.
    #[serde(default = "default_cfg")]
    pub cfg_scale: f32,
    /// sd.cpp sampler/scheduler name overrides ("" = the model's defaults;
    /// the request's `ancestral` flag picks euler_a/euler only when
    /// sample_method is unset here)
    #[serde(default)]
    pub sample_method: String,
    #[serde(default)]
    pub scheduler: String,
    /// when set, /v1/* requires `Authorization: Bearer <api_key>`. The web
    /// UI and /generate stay open - gate those with a PRIVATE deployment.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl AppConfig {
    /// Is this model's volume mounted on the deployment?
    pub fn volume_attached(&self) -> bool {
        std::path::Path::new(MODELS_ROOT)
            .join(&self.model_volume)
            .is_dir()
    }
}

fn default_cfg() -> f32 {
    1.0
}

/// Every model this deployment can serve, resolved and validated up front,
/// in catalog order.
pub struct Catalog {
    pub models: Vec<AppConfig>,
}

impl Catalog {
    /// The default for requests that don't name a model: the largest attached
    /// model (by max_size; later catalog entries win ties). When nothing is
    /// attached (dev boxes, error paths that still need a config to describe)
    /// the rule runs over the whole catalog instead.
    pub fn default_model(&self) -> &AppConfig {
        let attached: Vec<&AppConfig> =
            self.models.iter().filter(|m| m.volume_attached()).collect();
        let pool = if attached.is_empty() {
            self.models.iter().collect::<Vec<_>>()
        } else {
            attached
        };
        pool.into_iter()
            .reduce(|a, b| if b.max_size >= a.max_size { b } else { a })
            .expect("catalog resolves at least one model")
    }

    /// Resolve a request's model choice; None/"" means the default. Matches a
    /// display `name` or a `model_volume` (so `?model=qwen-image-2512` and
    /// `?model=qwen-image-2512-sd` both work).
    pub fn get(&self, name: Option<&str>) -> Result<&AppConfig, String> {
        let n = name.unwrap_or("").trim();
        if n.is_empty() {
            return Ok(self.default_model());
        }
        self.models
            .iter()
            .find(|m| m.name == n || m.model_volume == n)
            .ok_or_else(|| {
                format!(
                    "unknown model '{n}' (available: {})",
                    self.models
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// Embedded defaults with ENCLAVE_CONFIG overlaid, then the catalog resolved.
pub fn load() -> Result<Catalog, String> {
    catalog_from(load_raw()?)
}

fn load_raw() -> Result<Value, String> {
    let base: Value = serde_json::from_slice(APP_CONFIG_JSON)
        .map_err(|e| format!("embedded app-config.json: {e}"))?;
    match std::env::var("ENCLAVE_CONFIG") {
        Ok(over) if !over.trim().is_empty() => {
            let v: Value = serde_json::from_str(over.trim())
                .map_err(|e| format!("ENCLAVE_CONFIG is not valid JSON: {e}"))?;
            Ok(merge(base, v))
        }
        _ => Ok(base),
    }
}

/// Shallow key-wise overlay, except `models`: the catalog merges per volume
/// key, and each entry's fields merge shallowly - so an override can add one
/// model (or tweak one field of a known entry) without restating the rest.
fn merge(mut base: Value, over: Value) -> Value {
    let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) else {
        return over;
    };
    for (k, v) in o {
        if k == "models" {
            if let (Some(bm), Some(om)) = (
                b.get_mut("models").and_then(|m| m.as_object_mut()),
                v.as_object(),
            ) {
                for (vol, entry) in om {
                    match (bm.get_mut(vol).and_then(|e| e.as_object_mut()), entry.as_object()) {
                        (Some(be), Some(oe)) => {
                            for (ek, ev) in oe {
                                be.insert(ek.clone(), ev.clone());
                            }
                        }
                        _ => {
                            bm.insert(vol.clone(), entry.clone());
                        }
                    }
                }
                continue;
            }
        }
        b.insert(k.clone(), v.clone());
    }
    base
}

/// Resolve the catalog: one AppConfig per `models` map entry (keyed by
/// volume), or the top-level config itself when there is no map.
fn catalog_from(raw: Value) -> Result<Catalog, String> {
    let mut models = Vec::new();
    match raw.get("models") {
        Some(Value::Object(map)) if !map.is_empty() => {
            for (volume, entry) in map {
                models.push(resolve_entry(&raw, volume, entry.clone())?);
            }
        }
        None | Some(Value::Null) => models.push(resolve_top(&raw)?),
        Some(Value::Object(_)) => models.push(resolve_top(&raw)?), // empty map
        Some(_) => return Err("config 'models' must be a JSON object keyed by volume name".into()),
    }
    for i in 1..models.len() {
        if models[..i].iter().any(|m| m.name == models[i].name) {
            return Err(format!(
                "config: duplicate model name '{}' - request routing by name needs unique names",
                models[i].name
            ));
        }
    }
    Ok(Catalog { models })
}

/// The single model described by the top-level config (no catalog).
fn resolve_top(raw: &Value) -> Result<AppConfig, String> {
    let mut v = raw.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("models");
    }
    validate(v)
}

/// One catalog model: the entry's fields overlaid on the top-level template,
/// `model_volume` pinned to the volume KEY, and `name` from the entry
/// (defaulting to the key) - never inherited from the template, so entries
/// without a `name` stay distinct.
fn resolve_entry(raw: &Value, volume: &str, entry: Value) -> Result<AppConfig, String> {
    let name = entry
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(volume)
        .to_string();
    let mut base = raw.clone();
    let Some(b) = base.as_object_mut() else {
        return Err("app config must be a JSON object".into());
    };
    b.remove("models");
    if let Some(o) = entry.as_object() {
        for (k, v) in o {
            b.insert(k.clone(), v.clone());
        }
    } else {
        return Err(format!("config models['{volume}'] must be a JSON object"));
    }
    b.insert("name".into(), Value::String(name));
    b.insert("model_volume".into(), Value::String(volume.into()));
    validate(base)
}

fn validate(v: Value) -> Result<AppConfig, String> {
    let cfg: AppConfig = serde_json::from_value(v).map_err(|e| format!("app config: {e}"))?;
    if cfg.max_steps == 0 || cfg.default_steps == 0 || cfg.default_steps > cfg.max_steps {
        return Err(format!(
            "model '{}': steps config is inconsistent (default {} / max {})",
            cfg.name, cfg.default_steps, cfg.max_steps
        ));
    }
    if cfg.min_size > cfg.max_size || cfg.default_size < cfg.min_size || cfg.default_size > cfg.max_size {
        return Err(format!(
            "model '{}': size config is inconsistent (min {} / default {} / max {})",
            cfg.name, cfg.min_size, cfg.default_size, cfg.max_size
        ));
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded() -> Value {
        serde_json::from_slice(APP_CONFIG_JSON).unwrap()
    }

    #[test]
    fn map_catalog_maps_volume_to_name() {
        let cat = catalog_from(embedded()).unwrap();
        assert_eq!(cat.models.len(), 2);
        // the whole point: volume key -> display name, explicitly
        let zi = cat.get(Some("z-image-turbo")).unwrap();
        assert_eq!(zi.model_volume, "z-image-turbo-sd");
        assert_eq!(zi.default_steps, 4); // template default
        let qi = cat.get(Some("qwen-image-2512")).unwrap();
        assert_eq!(qi.model_volume, "qwen-image-2512-sd");
        assert_eq!(qi.default_steps, 8); // entry override
        assert_eq!(qi.min_size, 512); // entry override
        assert_eq!(qi.max_size, 1024); // inherited from template
    }

    #[test]
    fn selectable_by_name_or_volume() {
        let cat = catalog_from(embedded()).unwrap();
        // a request may name the model or the volume; both resolve
        assert_eq!(cat.get(Some("qwen-image-2512")).unwrap().model_volume, "qwen-image-2512-sd");
        assert_eq!(cat.get(Some("qwen-image-2512-sd")).unwrap().name, "qwen-image-2512");
        // nothing attached in tests -> largest overall, later entry wins ties
        assert_eq!(cat.default_model().name, "qwen-image-2512");
        let err = cat.get(Some("nope")).err().unwrap();
        assert!(err.contains("unknown model 'nope'"), "{err}");
        assert!(err.contains("z-image-turbo"), "{err}");
    }

    #[test]
    fn enclave_config_adds_one_model() {
        // a deployment adds a third model without restating the others
        let over = serde_json::json!({
            "models": { "flux2-klein-sd": { "name": "flux2-klein", "default_steps": 4, "min_size": 512 } }
        });
        let cat = catalog_from(merge(embedded(), over)).unwrap();
        assert_eq!(cat.models.len(), 3);
        let fk = cat.get(Some("flux2-klein")).unwrap();
        assert_eq!(fk.model_volume, "flux2-klein-sd");
        assert_eq!(fk.max_size, 1024); // still inherits the template
        // and the originals survive untouched
        assert_eq!(cat.get(Some("qwen-image-2512")).unwrap().default_steps, 8);
    }

    #[test]
    fn enclave_config_tweaks_one_field() {
        let over = serde_json::json!({ "models": { "qwen-image-2512-sd": { "default_steps": 6 } } });
        let cat = catalog_from(merge(embedded(), over)).unwrap();
        assert_eq!(cat.models.len(), 2); // no new model
        assert_eq!(cat.get(Some("qwen-image-2512")).unwrap().default_steps, 6);
        assert_eq!(cat.get(Some("qwen-image-2512")).unwrap().min_size, 512); // untouched
    }

    #[test]
    fn realistic_deploy_config_resolves() {
        // the App Config a deployment actually sets: a `volumes` list (read by
        // the platform, ignored by the app) plus a `models` map identical to
        // the embedded catalog. Must resolve to exactly the two models.
        let over = serde_json::json!({
            "volumes": ["qwen-image-2512-sd", "z-image-turbo-sd"],
            "models": {
                "z-image-turbo-sd":   { "name": "z-image-turbo" },
                "qwen-image-2512-sd": { "name": "qwen-image-2512", "default_steps": 8, "min_size": 512 }
            }
        });
        let cat = catalog_from(merge(embedded(), over)).unwrap();
        assert_eq!(cat.models.len(), 2);
        let zi = cat.get(Some("z-image-turbo")).unwrap();
        assert_eq!(zi.model_volume, "z-image-turbo-sd");
        assert_eq!((zi.default_steps, zi.max_size), (4, 1024));
        let qi = cat.get(Some("qwen-image-2512")).unwrap();
        assert_eq!(qi.model_volume, "qwen-image-2512-sd");
        assert_eq!((qi.default_steps, qi.min_size, qi.max_size), (8, 512, 1024));
        assert_eq!(cat.default_model().name, "qwen-image-2512"); // flagship
    }

    #[test]
    fn no_models_key_is_single_model() {
        let mut raw = embedded();
        raw.as_object_mut().unwrap().remove("models");
        let cat = catalog_from(raw).unwrap();
        assert_eq!(cat.models.len(), 1);
        assert_eq!(cat.default_model().name, "z-image-turbo");
        assert_eq!(cat.default_model().model_volume, "z-image-turbo-sd");
    }

    #[test]
    fn duplicate_names_rejected() {
        let mut raw = embedded();
        raw["models"] = serde_json::json!({ "a-sd": { "name": "x" }, "b-sd": { "name": "x" } });
        assert!(catalog_from(raw).err().unwrap().contains("duplicate model name"));
    }

    #[test]
    fn inconsistent_limits_rejected() {
        let mut raw = embedded();
        raw["models"] = serde_json::json!({ "a-sd": { "name": "a", "default_steps": 9 } });
        assert!(catalog_from(raw).err().unwrap().contains("steps config"));
        let mut raw = embedded();
        raw["models"] = serde_json::json!({ "a-sd": { "name": "a", "min_size": 4096 } });
        assert!(catalog_from(raw).err().unwrap().contains("size config"));
    }
}
