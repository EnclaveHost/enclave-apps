#!/usr/bin/env python3
"""Check bridge parity against the real Moonlight audio queue.

Usage: python3 tests/audio_fec_interop.py /path/to/moonlight-common-c
The checkout must include its nanors and enet submodules. No network access is
performed. The harness drops every possible pair of data packets, then checks
the client's recovered payloads, timestamps, and sequence numbers byte for byte.
"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
moonlight = Path(sys.argv[1]).resolve()
assert (moonlight / 'src/RtpAudioQueue.c').is_file()

C = r'''
#include "Limelight-internal.h"
#include <stdarg.h>
int AppVersionQuad[4] = {7, 1, 431, 0};
int AudioPacketDuration = 5;
static void log_message(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt); vfprintf(stderr, fmt, ap); va_end(ap);
}
CONNECTION_LISTENER_CALLBACKS ListenerCallbacks = {.logMessage = log_message};
uint64_t PltGetMicroseconds(void) { return 1000000; }
#include "RtpAudioQueue.c"
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "failed line %d: %s\n", __LINE__, #x); return 1; } } while (0)
static unsigned char shards[6][96];
static unsigned seen;
static uint16_t base;
static int consume(PRTP_PACKET p, uint16_t len) {
    unsigned i = (uint16_t)(p->sequenceNumber - base);
    CHECK(i < 4 && !(seen & (1u << i)));
    CHECK(len == sizeof(*p) + 96);
    if (p->header != 0x80 || p->packetType != 97 || p->ssrc != 0)
        fprintf(stderr, "header=%u type=%u ssrc=%u seq=%u len=%u\n", p->header, p->packetType, p->ssrc, p->sequenceNumber, len);
    CHECK(p->header == 0x80 && p->packetType == 97 && p->ssrc == 0);
    CHECK(p->timestamp == 100 + i * 5);
    CHECK(memcmp(p + 1, shards[i], 96) == 0);
    seen |= 1u << i;
    return 0;
}
int main(int argc, char **argv) {
    CHECK(argc == 2);
    FILE *f = fopen(argv[1], "rb"); CHECK(f);
    CHECK(fread(shards, 1, sizeof(shards), f) == sizeof(shards)); fclose(f);
    unsigned cases = 0;
    for (int wrap = 0; wrap < 2; wrap++) for (int a = 0; a < 4; a++) for (int b = a + 1; b < 4; b++) {
        RTP_AUDIO_QUEUE q; RtpaInitializeQueue(&q);
        base = wrap ? 65532 : 4; seen = 0;
        q.synchronizing = false;
        q.nextRtpSequenceNumber = q.oldestRtpBaseSequenceNumber = base;
        for (int i = 0; i < 6; i++) {
            if (i == a || i == b) continue;
            union { uint64_t alignment; unsigned char bytes[120]; } packet;
            memset(packet.bytes, 0, sizeof(packet.bytes));
            PRTP_PACKET p = (PRTP_PACKET)packet.bytes;
            p->header = 0x80; p->packetType = i < 4 ? 97 : 127;
            // AudioStream converts the outer RTP fields to host byte order
            // before calling the queue; FEC metadata remains network order.
            p->sequenceNumber = (uint16_t)(base + i);
            p->timestamp = i < 4 ? 100 + i * 5 : 0;
            uint16_t len = sizeof(*p) + 96;
            if (i < 4) memcpy(p + 1, shards[i], 96);
            else {
                PAUDIO_FEC_HEADER h = (PAUDIO_FEC_HEADER)(p + 1);
                h->fecShardIndex = i - 4; h->payloadType = 97;
                h->baseSequenceNumber = htons(base); h->baseTimestamp = htonl(100);
                memcpy(h + 1, shards[i], 96); len += sizeof(*h);
            }
            int result = RtpaAddPacket(&q, p, len);
            if (RTPQ_HANDLE_NOW(result)) CHECK(consume(p, len) == 0);
            PRTP_PACKET queued;
            while ((queued = RtpaGetQueuedPacket(&q, 0, &len))) {
                CHECK(consume(queued, len) == 0); free(queued);
            }
        }
        CHECK(!q.incompatibleServer && seen == 15);
        CHECK(q.stats.packetCountFecRecovered == 2);
        CHECK(q.stats.packetCountFecInvalid == 0 && q.stats.packetCountFecFailed == 0);
        RtpaCleanupQueue(&q); cases++;
    }
    printf("PASS: %u loss pairs including RTP sequence wrap; payloads and timestamps recovered exactly\n", cases);
    return 0;
}
'''

with tempfile.TemporaryDirectory(prefix='risc-audio-fec-') as scratch:
    p = Path(scratch)
    rust = '#[path = ' + json.dumps(str(root / 'src/fec.rs')) + '] mod fec;\n' + r'''
    use std::io::Write;
    fn main() {
        let data: Vec<Vec<u8>> = (0..4).map(|i| (0..96).map(|j| ((i*73+j*29+j*j)%256) as u8).collect()).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let parity = fec::encode_audio(&refs);
        let mut out = std::io::stdout().lock();
        for shard in data.iter().chain(parity.iter()) { out.write_all(shard).unwrap(); }
    }
    '''
    (p / 'fixture.rs').write_text(rust)
    subprocess.run(['rustc', '--edition=2021', '-Awarnings', str(p / 'fixture.rs'), '-o', str(p / 'fixture')], check=True)
    with (p / 'shards.bin').open('wb') as f:
        subprocess.run([str(p / 'fixture')], stdout=f, check=True)
    (p / 'check.c').write_text(C)
    n = moonlight / 'nanors'
    subprocess.run(['cc', '-O2',
                    '-I' + str(moonlight / 'src'), '-I' + str(moonlight / 'enet/include'),
                    '-I' + str(n), '-I' + str(n / 'deps/obl'), str(p / 'check.c'),
                    str(n / 'rs.c'), str(n / 'deps/obl/oblas_common.c'),
                    str(n / 'deps/obl/oblas_lite.c'), '-o', str(p / 'check')], check=True)
    subprocess.run([str(p / 'check'), str(p / 'shards.bin')], check=True)
