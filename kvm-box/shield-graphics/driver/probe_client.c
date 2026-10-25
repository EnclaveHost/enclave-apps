#include "shield_prepass_client.h"
#include <stdint.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc != 2) return 64;
    uint32_t input[256]; int32_t output[256];
    for (unsigned i = 0; i < 256; ++i) input[i] = 0xffff0000u | i;
    memset(output, 0xa5, sizeof output);
    enum shield_prepass_result result = shield_prepass_execute(input, output);
    enum shield_prepass_result expected = !strcmp(argv[1], "local") ? SHIELD_PREPASS_LOCAL : !strcmp(argv[1], "masked") ? SHIELD_PREPASS_MASKED : !strcmp(argv[1], "unavailable") ? SHIELD_PREPASS_UNAVAILABLE : SHIELD_PREPASS_FAILED;
    if (result != expected) { fprintf(stderr, "result %d != %d\n", result, expected); return 1; }
    if (result == SHIELD_PREPASS_LOCAL || result == SHIELD_PREPASS_MASKED) {
        for (unsigned row = 0; row < 64; ++row) {
            int32_t x = (int32_t)(row * 4) - 128, y = x + 1;
            int32_t values[4] = {x+y, y-x, x+2, x+3};
            if (memcmp(values, output + 4 * row, sizeof values)) return 2;
        }
    } else for (unsigned i = 0; i < sizeof output; ++i) if (((unsigned char *)output)[i] != 0xa5) return 3;
    printf("{\"passed\":true,\"result\":%d,\"gpuExecuted\":%s}\n", result, result == SHIELD_PREPASS_MASKED ? "true" : "false");
}
