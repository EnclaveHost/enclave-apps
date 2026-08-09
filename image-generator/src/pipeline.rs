//! txt2img over the host's stable-diffusion.cpp wasi-nn backend. The node
//! preloads every SD volume's checkpoint components at server startup
//! (`-S nn-graph=sd::<volume dir>` + the ENCLAVE_SD_*_FILE envs); the guest
//! opens it with load_by_name and ONE compute() runs the whole pipeline -
//! text encode, denoise, VAE decode - behind the wasi-nn boundary. Weights
//! never enter guest memory, so model size is bounded by the GPU share, not
//! wasm32; per-request traffic is the prompt in and raw RGB out.

use crate::bindings::wasi::nn::graph::load_by_name;
use crate::bindings::wasi::nn::tensor::{Tensor, TensorType};

use crate::config::AppConfig;

pub fn attached_volumes() -> Vec<String> {
    std::env::var("ENCLAVE_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn nn_err(stage: &str, e: crate::bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

// --------------------------------------------------------------- pipeline --

#[derive(Clone)]
pub struct GenRequest {
    pub prompt: String,
    pub negative_prompt: String,
    pub steps: usize,
    pub seed: u64,
    pub width: u32,
    pub height: u32,
    pub cfg: f32,
    pub ancestral: bool,
}

pub struct GenOutput {
    pub rgb: Vec<u8>, // HWC, width*height*3
    pub width: u32,
    pub height: u32,
    pub load_ms: u128,
    pub gen_ms: u128,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Run the full pipeline host-side. `status` receives progress lines;
/// returning false aborts (client gone). Statuses double as SSE keepalive
/// bytes through the long first-load silence.
pub fn generate(
    cfg: &AppConfig,
    req: &GenRequest,
    status: &mut dyn FnMut(&str) -> bool,
) -> Result<GenOutput, String> {
    if !status("opening the preloaded model") {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    let graph = load_by_name(&cfg.model_volume).map_err(|e| {
        format!(
            "{} (is the \"{}\" volume attached, and did the host preload it? \
             sdcpp needs `-S nn-graph=sd::<dir>` - a GPU-share deployment on a \
             node with the sd toolchain)",
            nn_err("load_by_name", e),
            cfg.model_volume
        )
    })?;
    let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
    let load_ms = now_ms() - t0;

    let method = if !cfg.sample_method.is_empty() {
        cfg.sample_method.clone()
    } else if req.ancestral {
        "euler_a".to_string()
    } else {
        "euler".to_string()
    };
    let scalar_i32 = |v: i32| Tensor::new(&[1], TensorType::I32, &v.to_le_bytes());
    let mut inputs: Vec<(String, Tensor)> = vec![
        (
            "prompt".into(),
            Tensor::new(&[1, req.prompt.len() as u32], TensorType::U8, req.prompt.as_bytes()),
        ),
        ("steps".into(), scalar_i32(req.steps as i32)),
        ("width".into(), scalar_i32(req.width as i32)),
        ("height".into(), scalar_i32(req.height as i32)),
        (
            "seed".into(),
            Tensor::new(&[1], TensorType::I64, &(req.seed as i64).to_le_bytes()),
        ),
        ("cfg".into(), Tensor::new(&[1], TensorType::Fp32, &req.cfg.to_le_bytes())),
        (
            "sample_method".into(),
            Tensor::new(&[1, method.len() as u32], TensorType::U8, method.as_bytes()),
        ),
    ];
    if !req.negative_prompt.is_empty() {
        inputs.push((
            "negative_prompt".into(),
            Tensor::new(
                &[1, req.negative_prompt.len() as u32],
                TensorType::U8,
                req.negative_prompt.as_bytes(),
            ),
        ));
    }
    if !cfg.scheduler.is_empty() {
        inputs.push((
            "scheduler".into(),
            Tensor::new(&[1, cfg.scheduler.len() as u32], TensorType::U8, cfg.scheduler.as_bytes()),
        ));
    }

    if !status(&format!(
        "generating {}x{} in {} step{} (host-side pipeline; blocks until done)",
        req.width,
        req.height,
        req.steps,
        if req.steps == 1 { "" } else { "s" },
    )) {
        return Err("client disconnected".into());
    }
    let t1 = now_ms();
    let outputs = ctx.compute(inputs).map_err(|e| nn_err("txt2img", e))?;
    let gen_ms = now_ms() - t1;
    let image = outputs
        .into_iter()
        .find(|(n, _)| n == "image")
        .map(|(_, t)| t)
        .ok_or("sd backend returned no \"image\" output")?;
    let rgb = image.data();
    if rgb.len() != (req.width * req.height * 3) as usize {
        return Err(format!(
            "sd backend returned {} bytes, expected {} ({}x{}x3)",
            rgb.len(),
            req.width * req.height * 3,
            req.width,
            req.height
        ));
    }
    Ok(GenOutput {
        rgb,
        width: req.width,
        height: req.height,
        load_ms,
        gen_ms,
    })
}

// --------------------------------------------------------------- upscaler --

pub struct UpscaleOutput {
    pub rgb: Vec<u8>, // HWC, width*height*3 at the UPSCALED size
    pub width: u32,
    pub height: u32,
    pub load_ms: u128,
    pub upscale_ms: u128,
}

/// Upscale an RGB image through an ESRGAN upscaler volume (host-side, one
/// compute(); the output tensor's dimensions carry the real factor). Same
/// status/abort contract as generate().
pub fn upscale(
    volume: &str,
    rgb: &[u8],
    width: u32,
    height: u32,
    status: &mut dyn FnMut(&str) -> bool,
) -> Result<UpscaleOutput, String> {
    if !status("opening the preloaded upscaler") {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    let graph = load_by_name(volume).map_err(|e| {
        format!(
            "{} (is the \"{volume}\" upscaler volume attached, and did the host \
             preload it? upscaler volumes need the node's ENCLAVE_SD_UPSCALE_FILE \
             env and a toolchain with the sd upscale verb)",
            nn_err("load_by_name", e),
        )
    })?;
    let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
    let load_ms = now_ms() - t0;

    if !status(&format!(
        "upscaling {width}x{height} (host-side ESRGAN, tiled; blocks until done)"
    )) {
        return Err("client disconnected".into());
    }
    let t1 = now_ms();
    let inputs = vec![(
        "image".to_string(),
        Tensor::new(&[1, height, width, 3], TensorType::U8, rgb),
    )];
    let outputs = ctx.compute(inputs).map_err(|e| nn_err("upscale", e))?;
    let upscale_ms = now_ms() - t1;
    let image = outputs
        .into_iter()
        .find(|(n, _)| n == "image")
        .map(|(_, t)| t)
        .ok_or("sd backend returned no \"image\" output")?;
    let (out_w, out_h) = match image.dimensions()[..] {
        [1, h, w, 3] => (w, h),
        ref d => return Err(format!("unexpected upscale output dimensions {d:?}")),
    };
    let rgb = image.data();
    if rgb.len() != (out_w as u64 * out_h as u64 * 3) as usize {
        return Err(format!(
            "upscaler returned {} bytes, expected {} ({out_w}x{out_h}x3)",
            rgb.len(),
            out_w as u64 * out_h as u64 * 3,
        ));
    }
    Ok(UpscaleOutput { rgb, width: out_w, height: out_h, load_ms, upscale_ms })
}

/// PNG-encode an RGB buffer (8-bit, no alpha).
pub fn to_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| format!("png data: {e}"))?;
    }
    Ok(out)
}

/// Decode a PNG into 8-bit RGB. Alpha is dropped, gray expands; 16-bit and
/// palette images normalize to 8-bit color first. `max_side` gates the
/// DECLARED dimensions before any frame buffer is allocated, so an
/// oversized (or decompression-bomb) upload fails on its header.
pub fn from_png(bytes: &[u8], max_side: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let mut limits = png::Limits::default();
    limits.bytes = 512 << 20;
    let mut dec = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| format!("not a decodable PNG: {e}"))?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if w == 0 || h == 0 || w > max_side || h > max_side {
        return Err(format!("image is {w}x{h}; this upscaler accepts up to {max_side}px a side"));
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let out = reader.next_frame(&mut buf).map_err(|e| format!("png decode: {e}"))?;
    buf.truncate(out.buffer_size());
    use png::ColorType::*;
    let rgb = match out.color_type {
        Rgb => buf,
        Rgba => buf.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0]]).collect(),
        other => return Err(format!("unsupported PNG color type {other:?}")),
    };
    Ok((rgb, out.width, out.height))
}
