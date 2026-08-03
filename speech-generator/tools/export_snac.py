#!/usr/bin/env python3
"""Export the SNAC 24 kHz decoder (hubertsiuzdak/snac_24khz) for the guest.

Reads the PyTorch checkpoint, fuses the weight-norm parametrization into plain
tensors (w = g * v / ||v||, norm over all dims but 0 - torch's dim=0 weight
norm, asserted against torch's own materialization), and writes src/snac.rs's
"SNACDEC1" container: f16 payload, self-describing tensor table, 26.2 MB.

Deterministic: same checkpoint in, byte-identical container out - which is why
fetch-model.sh can pin the OUTPUT's sha256 as well as the input's.

    python3 tools/export_snac.py <pytorch_model.bin> <snac_decoder.bin>

Needs torch and numpy only.

With --golden <dir>, also emits golden test vectors decoded by the OFFICIAL
implementation (pip install snac) with the noise blocks zeroed: random code
streams and their f32 audio, for validating src/snac.rs after any change to
its math. The Rust decoder reproduces them at 52-59 dB SNR (the f16
quantization floor); chunked and frame-stitched decode are bit-exact against
full decode.

Container format (little-endian):
  magic  b"SNACDEC1"
  u32    n_tensors
  per tensor:
    u16   name_len, name (utf-8)
    u8    dtype (0=f16, 1=f32)
    u8    ndim
    u32   dims[ndim]
    u64   payload bytes
    data
"""

import struct
import sys

import numpy as np
import torch


def fuse(sd, prefix):
    g = sd[f"{prefix}.parametrizations.weight.original0"]
    v = sd[f"{prefix}.parametrizations.weight.original1"]
    return g * v / v.norm(dim=(1, 2), keepdim=True)


def collect(sd):
    """name -> tensor, in the fixed order the container is written in."""
    tensors = {}
    for q in range(3):
        p = f"quantizer.quantizers.{q}"
        tensors[f"q{q}.codebook"] = sd[f"{p}.codebook.weight"]
        tensors[f"q{q}.out_proj.w"] = fuse(sd, f"{p}.out_proj")
        tensors[f"q{q}.out_proj.b"] = sd[f"{p}.out_proj.bias"]

    def conv(name, prefix, bias=True):
        tensors[f"{name}.w"] = fuse(sd, prefix)
        if bias:
            tensors[f"{name}.b"] = sd[f"{prefix}.bias"]

    conv("dec.in_dw", "decoder.model.0")  # depthwise (768,1,7)
    conv("dec.in_pw", "decoder.model.1")  # pointwise (1024,768,1)
    for bi, m in enumerate([2, 3, 4, 5]):
        b, base = f"blk{bi}", f"decoder.model.{m}.block"
        tensors[f"{b}.snake_in.alpha"] = sd[f"{base}.0.alpha"]
        conv(f"{b}.tconv", f"{base}.1")  # (Cin, Cout, 2*stride)
        tensors[f"{b}.noise.w"] = fuse(sd, f"{base}.2.linear")  # (C,C,1) no bias
        for ru, ri in enumerate([3, 4, 5]):
            r, rbase = f"{b}.res{ru}", f"{base}.{ri}.block"
            tensors[f"{r}.snake1.alpha"] = sd[f"{rbase}.0.alpha"]
            conv(f"{r}.dw", f"{rbase}.1")  # depthwise (C,1,7) dilated
            tensors[f"{r}.snake2.alpha"] = sd[f"{rbase}.2.alpha"]
            conv(f"{r}.pw", f"{rbase}.3")  # pointwise (C,C,1)
    tensors["dec.out_snake.alpha"] = sd["decoder.model.6.alpha"]
    conv("dec.out", "decoder.model.7")  # (1, 64, 7)
    return tensors


def sanity_check_fusing(sd, tensors):
    """The fused pointwise conv must match torch's own weight-norm forward."""
    ref = torch.nn.Conv1d(768, 1024, 1)
    ref.weight.data = sd["decoder.model.1.parametrizations.weight.original1"].clone()
    torch.nn.utils.parametrizations.weight_norm(ref, dim=0)
    ref.parametrizations.weight.original0.data = sd[
        "decoder.model.1.parametrizations.weight.original0"
    ]
    ref.parametrizations.weight.original1.data = sd[
        "decoder.model.1.parametrizations.weight.original1"
    ]
    err = (ref.weight - tensors["dec.in_pw.w"]).abs().max().item()
    assert err < 1e-6, f"weight-norm fuse err {err}"


def write_container(path, tensors):
    with open(path, "wb") as f:
        f.write(b"SNACDEC1")
        f.write(struct.pack("<I", len(tensors)))
        for name, t in tensors.items():
            a = t.detach().numpy().astype(np.float16)
            nb = name.encode()
            f.write(struct.pack("<H", len(nb)))
            f.write(nb)
            f.write(struct.pack("<BB", 0, a.ndim))
            f.write(struct.pack(f"<{a.ndim}I", *a.shape))
            f.write(struct.pack("<Q", a.nbytes))
            f.write(a.tobytes())


def emit_golden(out_dir, ckpt_path):
    import json
    import os

    from snac import SNAC
    import snac.layers as layers

    model = SNAC.from_pretrained("hubertsiuzdak/snac_24khz").eval()
    model.load_state_dict(torch.load(ckpt_path, map_location="cpu", weights_only=True))
    noiseless = lambda self, x: x  # noqa: E731 - zero the noise for determinism
    orig, layers.NoiseBlock.forward = layers.NoiseBlock.forward, noiseless
    os.makedirs(out_dir, exist_ok=True)
    rng = np.random.default_rng(42)
    meta = {}
    try:
        for name, n_frames in {"tiny": 4, "short": 16, "med": 96}.items():
            codes = [
                torch.tensor(rng.integers(0, 4096, (1, k * n_frames)), dtype=torch.long)
                for k in (1, 2, 4)
            ]
            with torch.no_grad():
                audio = model.decoder(model.quantizer.from_codes(codes))
            for i in range(3):
                codes[i].numpy().astype(np.uint16).tofile(f"{out_dir}/{name}_codes{i}.u16")
            a = audio.squeeze().numpy().astype(np.float32)
            a.tofile(f"{out_dir}/{name}_audio.f32")
            meta[name] = {"frames": n_frames, "samples": int(a.size),
                          "rms": float(np.sqrt((a ** 2).mean()))}
            print(f"golden {name}: {n_frames} frames -> {a.size} samples")
    finally:
        layers.NoiseBlock.forward = orig
    with open(f"{out_dir}/meta.json", "w") as f:
        json.dump(meta, f, indent=2)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 2:
        print(__doc__)
        return 1
    ckpt, out = args
    sd = torch.load(ckpt, map_location="cpu", weights_only=True)
    tensors = collect(sd)
    sanity_check_fusing(sd, tensors)
    write_container(out, tensors)
    n_params = sum(t.numel() for t in tensors.values())
    print(f"wrote {out}: {len(tensors)} tensors, {n_params/1e6:.2f}M params (f16)")
    if "--golden" in sys.argv:
        i = sys.argv.index("--golden")
        emit_golden(sys.argv[i + 1], ckpt)
    return 0


if __name__ == "__main__":
    sys.exit(main())
