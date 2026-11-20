#pragma OPENCL EXTENSION cl_khr_fp16 : enable

//------------------------------------------------------------------------------
// kernel_mul_mat_q3_k_f32_raw
//
// A plain product for Q3_K weights kept exactly as the GGUF holds them: 110
// bytes for each 256 weights (32 bytes with the high bit of each quant, 64
// bytes with the low two bits, 12 bytes of 6-bit scales, half d). Sixteen
// parts of 16 weights; a weight is d * (scale - 32) * (quant - 4), the quant
// being three bits.
// One work item computes one output, dst[r][j] = row j of src0 . row r of src1.
// Written to be read, not to be fast: the Enclave Shield device offloads this
// product when it can and only runs the kernel when it cannot.
//------------------------------------------------------------------------------
kernel void kernel_mul_mat_q3_k_f32_raw(
    global uchar * src0,
    ulong offset0,
    global float * src1,
    ulong offset1,
    global float * dst,
    ulong offsetd,
    int ne00,
    int ne01,
    int ne11
) {
    src0 = src0 + offset0;
    src1 = (global float *)((global char *)src1 + offset1);
    dst  = (global float *)((global char *)dst + offsetd);

    const int j = get_global_id(0);
    const int r = get_global_id(1);
    if (j < ne01 && r < ne11) {
        const int nb = ne00 / 256;
        global uchar * row = src0 + (ulong)j * (ulong)nb * 110;
        global float * x = src1 + (ulong)r * (ulong)ne00;

        float total = 0.0f;
        for (int ib = 0; ib < nb; ib++) {
            global uchar * block = row + (ulong)ib * 110;
            global uchar * high = block;
            global uchar * quants = block + 32;
            global uchar * scales = block + 96;
            const float d = vload_half(0, (global half *)(block + 108));
            for (int part = 0; part < 16; part++) {
                const int low = part < 8 ? scales[part] & 0xF : scales[part - 8] >> 4;
                const int scale = (low | (((scales[8 + part % 4] >> (2 * (part / 4))) & 3) << 4)) - 32;
                // Each half block has 32 quant bytes; a byte holds one weight
                // of each of four pairs of parts, and the high-bit bytes one
                // bit for each of the eight pairs.
                const int half_block = part / 8, pair = (part % 8) / 2;
                global uchar * q = quants + half_block * 32 + (part % 2) * 16;
                global uchar * h = high + (part % 2) * 16;
                const int bit = 1 << (half_block * 4 + pair);
                global float * y = x + ib * 256 + part * 16;
                float sum = 0.0f;
                for (int i = 0; i < 16; i++) {
                    const int value = ((q[i] >> (2 * pair)) & 3) - ((h[i] & bit) ? 0 : 4);
                    sum += y[i] * (float)value;
                }
                total += d * (float)scale * sum;
            }
        }
        dst[(ulong)r * (ulong)ne01 + j] = total;
    }
}
