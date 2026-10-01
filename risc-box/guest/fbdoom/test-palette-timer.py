#!/usr/bin/env python3
"""Generate a real RV64 regression executable for the exact replacement bytes.

python3 test-palette-timer.py /tmp/timer-regression.c
riscv64-linux-gnu-gcc -static -O2 /tmp/timer-regression.c -o /tmp/timer-regression
qemu-riscv64 /tmp/timer-regression
"""
import importlib.util
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("timer", Path(__file__).with_name("patch-palette-timer.py"))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def function(name, data, elapsed=False):
    # The actual I_GetTime span includes its epilogue. Reproduce its entry
    # registers/stack; do not translate/reimplement the instructions under test.
    pre = "addi sp,sp,-48; sd ra,40(sp); mv a5,a0; li a2,1000; " if elapsed else ""
    post = "" if elapsed else "mv a0,a5; ret; "
    bytecode = ",".join(str(v) for v in data)
    return f'__asm__(".text; .balign 2; .global {name}; {name}: {pre}.byte {bytecode}; {post}");\n'


source = """
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
extern long adjusted_old(int), adjusted_new(int);
extern long elapsed_old(int, int), elapsed_new(int, int);
int main(void) {
    // 2^31/35 ms: signed overflow froze NetUpdate at about 17 hours.
    int boundary = INT32_MAX / 35;
    assert(adjusted_old(boundary + 1) != (int64_t)(boundary + 1) * 35 / 1000);
    // 2^32/35 ms: the independent I_GetTime clock wrapped at ~34 hours.
    uint32_t ub = UINT32_MAX / 35 + 1;
    assert(elapsed_old((int)ub, 0) != (uint64_t)ub * 35 / 1000);
    int cases[] = {-1000, 0, 1, 999, 1000, 61200000, 61356675, 61356676,
                   86400000, 122713351, 122713352, 144000000, INT32_MAX};
    for (unsigned i = 0; i < sizeof(cases)/sizeof(cases[0]); i++)
        assert(adjusted_new(cases[i]) == (int64_t)cases[i] * 35 / 1000);
    // Wide multiply after unsigned modular subtraction, including clock wrap.
    uint32_t seed = 0x12345678;
    for (int i = 0; i < 100000; i++) {
        seed = seed * 1664525u + 1013904223u;
        uint32_t now = seed, base = seed ^ 0xabcdef12u;
        uint32_t elapsed = now - base;
        assert(elapsed_new((int)now, (int)base) == (uint64_t)elapsed * 35 / 1000);
        int ms = (int)(now & INT32_MAX);
        assert(adjusted_new(ms) == (int64_t)ms * 35 / 1000);
    }
    puts("PASS: original overflow reproduced; 200013 wide timer cases passed");
}
"""
for suffix in ("old", "new"):
    source += function("adjusted_" + suffix, getattr(m, "ADJUSTED_" + suffix.upper()))
    source += function("elapsed_" + suffix, getattr(m, "ELAPSED_" + suffix.upper()), True)
Path(sys.argv[1]).write_text(source)
