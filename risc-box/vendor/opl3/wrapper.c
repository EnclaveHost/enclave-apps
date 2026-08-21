// risc-box: the host half of the machine's OPL music.
//
// The guest writes OPL registers to the emulator's mailbox at 0x10006000
// (emu/src/device/opl.rs); this applies them to a real Nuked OPL3 chip and
// generates the samples. Native code doing the per-sample work is the entire
// point: the same synthesis inside the guest cost 21% of DOOM's frame rate,
// because a cycle-accurate 49716 Hz chip on an emulated RV64 core with no
// vector unit is exactly the sort of thing that machine is worst at.
//
// Freestanding wasm: opl3.c needs only memset/memcpy, which vendor's shim
// provides, and its one math.h use sits behind OPL_ENABLE_STEREOEXT, left off.

#include <stdint.h>
#include "opl3.h"

static opl3_chip chip;
static int ready = 0;

void rbx_opl_init(uint32_t rate)
{
    OPL3_Reset(&chip, rate);
    ready = 1;
}

void rbx_opl_write(uint16_t reg, uint8_t value)
{
    if (ready)
    {
        // Buffered: matches how a real chip latches a register write, and is
        // what chocolate-doom's own driver uses.
        OPL3_WriteRegBuffered(&chip, reg, value);
    }
}

/// Generate `frames` STEREO frames (2 int16 each) of music.
void rbx_opl_generate(int16_t *out, uint32_t frames)
{
    if (!ready)
    {
        for (uint32_t i = 0; i < frames * 2; ++i)
        {
            out[i] = 0;
        }
        return;
    }
    OPL3_GenerateStream(&chip, out, frames);
}
