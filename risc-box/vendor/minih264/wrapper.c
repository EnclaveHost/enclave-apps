/* Flat, allocation-free entry points over minih264 for the Rust FFI in
 * src/video.rs. The encoder's state lives in two caller-provided buffers
 * (persist + scratch), so this file never allocates: Rust owns the memory,
 * C owns the layout. Compiled freestanding (shim/ headers) because the wasm
 * targets have no C sysroot at cargo time; memcpy/memset resolve against the
 * wasi-libc the final link already carries.
 *
 * minih264e.h is vendored verbatim from github.com/lieff/minih264 (CC0)
 * plus one marked one-line patch: wasm added to the little-endian branch of
 * its platform probe, which otherwise ends at #error.
 */
#define MINIH264_IMPLEMENTATION
#define H264E_MAX_THREADS 0
#include "minih264e.h"

static void fill_create(H264E_create_param_t *cp, int w, int h, int gop, int kbps)
{
    memset(cp, 0, sizeof *cp);
    cp->width = w;
    cp->height = h;
    cp->gop = gop;
    /* Half a second of VBV: frame-level rate control adapts on VBV feedback,
     * and a full second let a hard view rotation run 8-9 Mbps against a
     * 2.4 Mbps session for ~500 ms — a wire backlog the viewer feels as a
     * freeze the moment the motion stops. */
    cp->vbv_size_bytes = kbps * 1000 / 16;
    /* Macroblock-level rate control: holds the byte budget WITHIN a frame
     * instead of noticing the overshoot one frame later. Slightly lower
     * quality at a given rate; a real-time stream cares more that the rate
     * is real. */
    cp->fine_rate_control_flag = 1;
    /* The input planes are Rust-owned slices; the encoder must not scribble
     * on them (aliasing rules), so it gets its own internal copy buffer. */
    cp->const_input_flag = 1;
}

int rbx_h264_sizeof(int w, int h, int gop, int kbps, int *persist, int *scratch)
{
    H264E_create_param_t cp;
    fill_create(&cp, w, h, gop, kbps);
    return H264E_sizeof(&cp, persist, scratch);
}

int rbx_h264_init(void *persist, int w, int h, int gop, int kbps)
{
    H264E_create_param_t cp;
    fill_create(&cp, w, h, gop, kbps);
    return H264E_init((H264E_persist_t *)persist, &cp);
}

/* Encode one I420 frame. Returns minih264's status (0 = ok); *coded points
 * into the scratch buffer and is valid until the next call. force_key makes
 * this frame a random-access point (SPS+PPS+IDR). */
int rbx_h264_encode(void *persist, void *scratch,
                    const unsigned char *y, const unsigned char *u, const unsigned char *v,
                    int w, int h, int force_key, int desired_bytes, int qp_min, int qp_max,
                    const unsigned char **coded, int *coded_len)
{
    H264E_run_param_t rp;
    H264E_io_yuv_t io;
    unsigned char *out = 0;
    int outlen = 0;
    int rc;
    (void)h;

    memset(&rp, 0, sizeof rp);
    rp.encode_speed = H264E_SPEED_FASTEST;
    rp.frame_type = force_key ? H264E_FRAME_TYPE_KEY : H264E_FRAME_TYPE_DEFAULT;
    rp.desired_frame_bytes = desired_bytes;
    rp.qp_min = qp_min;
    rp.qp_max = qp_max;

    io.yuv[0] = (unsigned char *)y; io.stride[0] = w;
    io.yuv[1] = (unsigned char *)u; io.stride[1] = w / 2;
    io.yuv[2] = (unsigned char *)v; io.stride[2] = w / 2;

    rc = H264E_encode((H264E_persist_t *)persist, (H264E_scratch_t *)scratch,
                      &rp, &io, &out, &outlen);
    *coded = out;
    *coded_len = outlen;
    return rc;
}
