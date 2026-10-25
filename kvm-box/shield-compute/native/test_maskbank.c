/* White-box boundary test of the exact production C source. No production
 * diagnostic hooks or configurable pad limits are added. Never ship this test.
 */
#include "shielded-tee.c"
#include <assert.h>

typedef struct { sh_maskbank *bank; int result; int32_t pad[64]; } call;
static void *issue(void *opaque) {
    call *c = opaque;
    for (int i = 0; i < 64; ++i) c->pad[i] = INT32_MIN;
    c->result = maskbank_issue(c->bank, c->pad, 64);
    return NULL;
}

int main(void) {
    sh_maskbank bank;
    assert(maskbank_init(&bank));
    const uint64_t capacity = UINT64_C(1) << 40;
    assert(bank.capacity == capacity);
    // Advance to the final eight production issuance windows, without
    // changing the capacity, key, generator, lock or implementation.
    bank.counter = bank.issued_hi = capacity - 8;
    call calls[16]; pthread_t threads[16];
    for (int i = 0; i < 16; ++i) {
        calls[i].bank = &bank;
        assert(!pthread_create(&threads[i], NULL, issue, &calls[i]));
    }
    int accepted = 0, exhausted = 0;
    for (int i = 0; i < 16; ++i) {
        assert(!pthread_join(threads[i], NULL));
        if (calls[i].result == SH_OK) {
            accepted++;
            for (int j = 0; j < 64; ++j) assert(calls[i].pad[j] >= 0 && calls[i].pad[j] < SH_M_MOD);
            for (int j = 0; j < i; ++j)
                if (calls[j].result == SH_OK) assert(memcmp(calls[i].pad, calls[j].pad, sizeof calls[i].pad));
        } else {
            assert(calls[i].result == SH_ERR_EXHAUST); exhausted++;
            for (int j = 0; j < 64; ++j) assert(calls[i].pad[j] == INT32_MIN);
        }
    }
    assert(accepted == 8 && exhausted == 8 && bank.counter == capacity && bank.issued_hi == capacity);
    call retry = {.bank = &bank};
    issue(&retry); assert(retry.result == SH_ERR_EXHAUST);
    for (int j = 0; j < 64; ++j) assert(retry.pad[j] == INT32_MIN);
    assert(bank.counter == capacity && bank.issued_hi == capacity);
    int32_t sentinel = INT32_MIN;
    assert(maskbank_issue(&bank, &sentinel, (UINT64_C(1) << 27) + 1) == SH_ERR_RANGE);
    assert(sentinel == INT32_MIN && bank.counter == capacity);
    pthread_mutex_destroy(&bank.mu);
    puts("{\"capacity\":1099511627776,\"acceptedFinalWindows\":8,\"exhaustedConcurrentCalls\":8,\"retryRejected\":true,\"outputsUntouched\":true,\"oversizedWindowRejected\":true}");
    return 0;
}
