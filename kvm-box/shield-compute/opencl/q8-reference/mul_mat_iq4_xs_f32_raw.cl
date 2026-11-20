#pragma OPENCL EXTENSION cl_khr_fp16 : enable

//------------------------------------------------------------------------------
// kernel_mul_mat_iq4_xs_f32_raw
//
// A plain product for IQ4_XS weights kept exactly as the GGUF holds them:
// 136 bytes for each 256 weights (half d, 16 bits of high scale bits, four
// bytes of low scale bits, 128 bytes of 4-bit indices into a fixed table).
// One work item computes one output, dst[r][j] = row j of src0 . row r of src1.
// Written to be read, not to be fast: the Enclave Shield device offloads this
// product when it can and only runs the kernel when it cannot.
//------------------------------------------------------------------------------
kernel void kernel_mul_mat_iq4_xs_f32_raw(
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

    const char values[16] = {-127, -104, -83, -65, -49, -35, -22, -10, 1, 13, 25, 38, 53, 69, 89, 113};

    const int j = get_global_id(0);
    const int r = get_global_id(1);
    if (j < ne01 && r < ne11) {
        const int nb = ne00 / 256;
        global uchar * row = src0 + (ulong)j * (ulong)nb * 136;
        global float * x = src1 + (ulong)r * (ulong)ne00;

        float total = 0.0f;
        for (int ib = 0; ib < nb; ib++) {
            global uchar * block = row + (ulong)ib * 136;
            const float d = vload_half(0, (global half *)block);
            const uint high = (uint)block[2] | ((uint)block[3] << 8);
            for (int part = 0; part < 8; part++) {
                const int low = (block[4 + part / 2] >> (4 * (part % 2))) & 0xF;
                const int scale = (low | (((high >> (2 * part)) & 3) << 4)) - 32;
                global uchar * q = block + 8 + part * 16;
                global float * y = x + ib * 256 + part * 32;
                float sum = 0.0f;
                for (int i = 0; i < 16; i++) {
                    sum += y[i] * (float)values[q[i] & 0xF];
                    sum += y[i + 16] * (float)values[q[i] >> 4];
                }
                total += d * (float)scale * sum;
            }
        }
        dst[(ulong)r * (ulong)ne01 + j] = total;
    }
}
