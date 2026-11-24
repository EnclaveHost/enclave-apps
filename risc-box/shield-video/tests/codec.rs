#![cfg(feature = "codec-tests")]
use enclave_shield_video::{with_frame, Client};
extern "C" {
    fn rbx_h264_sizeof(w: i32, h: i32, gop: i32, kbps: i32, p: *mut i32, s: *mut i32) -> i32;
    fn rbx_h264_init(p: *mut u8, w: i32, h: i32, gop: i32, kbps: i32) -> i32;
    fn rbx_h264_encode(
        p: *mut u8,
        s: *mut u8,
        y: *const u8,
        u: *const u8,
        v: *const u8,
        w: i32,
        h: i32,
        key: i32,
        bytes: i32,
        qmin: i32,
        qmax: i32,
        out: *mut *const u8,
        len: *mut i32,
    ) -> i32;
}
struct Encoder {
    p: Vec<u64>,
    s: Vec<u64>,
    w: usize,
    h: usize,
}
impl Encoder {
    fn new(w: usize, h: usize) -> Self {
        unsafe {
            let (mut p, mut s) = (0, 0);
            assert_eq!(
                rbx_h264_sizeof(w as i32, h as i32, 0, 3000, &mut p, &mut s),
                0
            );
            let mut enc = Self {
                p: vec![0; (p as usize + 7) / 8],
                s: vec![0; (s as usize + 7) / 8],
                w,
                h,
            };
            assert_eq!(
                rbx_h264_init(enc.p.as_mut_ptr().cast(), w as i32, h as i32, 0, 3000),
                0
            );
            enc
        }
    }
    fn encode(&mut self, y: &[u8], u: &[u8], v: &[u8], key: bool) -> Vec<u8> {
        unsafe {
            let mut out = std::ptr::null();
            let mut len = 0;
            assert_eq!(
                rbx_h264_encode(
                    self.p.as_mut_ptr().cast(),
                    self.s.as_mut_ptr().cast(),
                    y.as_ptr(),
                    u.as_ptr(),
                    v.as_ptr(),
                    self.w as i32,
                    self.h as i32,
                    key as i32,
                    9000,
                    10,
                    48,
                    &mut out,
                    &mut len
                ),
                0
            );
            assert!(len > 0 && !out.is_null());
            std::slice::from_raw_parts(out, len as usize).to_vec()
        }
    }
}
#[test]
#[ignore = "requires a dedicated Enclave Shield GPU worker; SHIELD_VIDEO_WORKER selects it"]
fn real_gpu_bitstream_matches_cpu() {
    run_cases();
}

pub fn run_cases() {
    let addr = std::env::var("SHIELD_VIDEO_WORKER").expect("explicit dedicated worker required");
    let cases = if std::env::var_os("SHIELD_VIDEO_LARGE_TEST").is_some() {
        vec![(1024, 768)]
    } else {
        vec![(64, 64), (96, 64), (66, 50)]
    };
    for (w, h) in cases {
        let mut client = Client::connect(&addr).unwrap();
        let mut cpu = Encoder::new(w, h);
        let mut gpu = Encoder::new(w, h);
        let mut stream = Vec::new();
        let mut hits = 0;
        let mut misses = 0;
        let start = std::time::Instant::now();
        let mut cpu_time = std::time::Duration::ZERO;
        let mut masked_time = std::time::Duration::ZERO;
        for frame in 0..8 {
            let y: Vec<u8> = (0..w * h)
                .map(|i| {
                    if frame == 0 {
                        128
                    } else {
                        ((i * 17 + i / w * 11 + frame * 31) % 256) as u8
                    }
                })
                .collect();
            let u: Vec<u8> = (0..w * h / 4)
                .map(|i| ((i * 13 + frame * 7) % 256) as u8)
                .collect();
            let v = vec![110; w * h / 4];
            let timer = std::time::Instant::now();
            let expected = cpu.encode(&y, &u, &v, frame == 0 || frame == 5);
            cpu_time += timer.elapsed();
            let timer = std::time::Instant::now();
            let cache = client.frame(&y, &u, &v, w, h).unwrap();
            let (actual, hit, miss) =
                with_frame(cache, || gpu.encode(&y, &u, &v, frame == 0 || frame == 5)).unwrap();
            masked_time += timer.elapsed();
            hits += hit;
            misses += miss;
            assert_eq!(
                actual, expected,
                "bitstream differs at {w}x{h} frame {frame}"
            );
            stream.extend(actual);
        }
        assert!(hits > 0, "GPU coefficients must actually reach the codec");
        let output = std::env::var_os("SHIELD_VIDEO_OUTPUT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "enclave-shield-video-{}-{w}x{h}.h264",
                if cfg!(target_arch = "wasm32") {
                    0
                } else {
                    std::process::id()
                }
            ));
        std::fs::write(&output, stream).unwrap();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let decode = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&output)
                .args(["-f", "null", "-"])
                .output()
                .unwrap();
            assert!(
                decode.status.success(),
                "{}",
                String::from_utf8_lossy(&decode.stderr)
            );
            assert!(
                decode.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&decode.stderr)
            );
            std::fs::remove_file(output).unwrap();
        }
        eprintln!("{w}x{h}: 8 frames exact (native also decodes); hits={hits} misses={misses} exchanges={} elapsed={:?} cpu={cpu_time:?} masked={masked_time:?}",client.exchanges,start.elapsed());
    }
}
