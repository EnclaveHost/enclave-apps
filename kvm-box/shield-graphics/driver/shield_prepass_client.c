/* Guest-local transport only. Approval, masking and verification remain in the
 * existing measured Shield service. No worker endpoint or GPU handle exists
 * in this client. Mesa admission must establish the exact shader contract. */
#define _GNU_SOURCE
#include "shield_prepass_client.h"
#include <arpa/inet.h>
#include <errno.h>
#include <json-c/json.h>
#include <poll.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define MAX_ROWS 4096
#define TIMEOUT_MS 15000
#define SERVICE_UID 1001
#define SERVICE_SOCKET "/run/shield/compute.sock"

static int64_t now_ms(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts)) return -1;
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}
static bool transfer(int fd, void *data, size_t length, bool writing, int64_t deadline) {
    unsigned char *bytes = data;
    while (length) {
        int64_t now = now_ms();
        if (now < 0 || now >= deadline) return false;
        struct pollfd p = { .fd = fd, .events = writing ? POLLOUT : POLLIN };
        int ready = poll(&p, 1, (int)(deadline - now));
        if (ready < 0 && errno == EINTR) continue;
        if (ready <= 0 || !(p.revents & p.events)) return false;
        ssize_t n = writing ? send(fd, bytes, length, MSG_NOSIGNAL | MSG_DONTWAIT)
                            : recv(fd, bytes, length, MSG_DONTWAIT);
        if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
        if (n <= 0) return false;
        bytes += n; length -= (size_t)n;
    }
    return true;
}
static bool append(char *data, size_t capacity, size_t *used, const char *text) {
    size_t n = strlen(text);
    if (n >= capacity - *used) return false;
    memcpy(data + *used, text, n + 1); *used += n; return true;
}
static bool number(char *data, size_t capacity, size_t *used, int value) {
    char text[24]; int n = snprintf(text, sizeof text, "%d", value);
    return n > 0 && (size_t)n < sizeof text && append(data, capacity, used, text);
}
static size_t request(const struct shield_affine64 *transform, const uint32_t *input, unsigned count, char *data, size_t capacity) {
    size_t used = 0;
    if (!count || count > MAX_ROWS || !capacity) return 0;
    if (!append(data, capacity, &used, "{\"op\":\"graph\",\"mode\":\"masked\",\"graph\":{\"schema\":\"shield-graph/1\",\"nodes\":[{\"name\":\"x\",\"op\":\"input\",\"shape\":[")) return 0;
    if (!number(data, capacity, &used, (int)count)) return 0;
    if (!append(data, capacity, &used, ",32],\"bound\":128},{\"name\":\"w\",\"op\":\"constant\",\"shape\":[32,4],\"value\":[")) return 0;
    for (unsigned row = 0; row < 32; ++row) {
        if (!append(data, capacity, &used, row ? ",[" : "[")) return 0;
        for (unsigned col = 0; col < 4; ++col) {
            if (col && !append(data, capacity, &used, ",")) return 0;
            if (!number(data, capacity, &used, row < 4 ? transform->matrix[row][col] : 0)) return 0;
        }
        if (!append(data, capacity, &used, "]")) return 0;
    }
    if (!append(data, capacity, &used, "]},{\"name\":\"y\",\"op\":\"matmul\",\"args\":[\"x\",\"w\"]}],\"outputs\":[\"y\"]},\"inputs\":{\"x\":[")) return 0;
    for (unsigned row = 0; row < count; ++row) {
        if (!append(data, capacity, &used, row ? ",[" : "[")) return 0;
        for (unsigned col = 0; col < 32; ++col) {
            if (col && !append(data, capacity, &used, ",")) return 0;
            int value = col < 4 ? (int)(input[row * 4 + col] & 255u) - 128 : 0;
            if (!number(data, capacity, &used, value)) return 0;
        }
        if (!append(data, capacity, &used, "]")) return 0;
    }
    return append(data, capacity, &used, "]}}") ? used : 0;
}
static bool field(struct json_object *object, const char *name, enum json_type type, struct json_object **value) {
    return json_object_is_type(object, json_type_object) && json_object_object_get_ex(object, name, value) && json_object_is_type(*value, type);
}
static bool counter(struct json_object *object, const char *name, int64_t min, int64_t max) {
    struct json_object *value;
    if (!field(object, name, json_type_int, &value)) return false;
    int64_t count = json_object_get_int64(value);
    return count >= min && count <= max;
}
static enum shield_prepass_result response(const struct shield_affine64 *transform, const char *data, size_t length, unsigned count, int32_t *output) {
    enum shield_prepass_result result = SHIELD_PREPASS_FAILED;
    if (!count || count > MAX_ROWS || length > 2048u+count*32u) return result;
    struct json_tokener *parser = json_tokener_new_ex(16);
    if (!parser) return result;
    json_tokener_set_flags(parser, JSON_TOKENER_STRICT | JSON_TOKENER_VALIDATE_UTF8);
    struct json_object *root = json_tokener_parse_ex(parser, data, (int)length);
    bool parsed = json_tokener_get_error(parser) == json_tokener_success && json_tokener_get_parse_end(parser) == length;
    json_tokener_free(parser);
    // service.send emits compact ASCII JSON for this response. Re-encoding
    // rejects duplicate keys and ambiguous/noncanonical encodings that a JSON
    // object parser would otherwise silently collapse or normalize.
    if (parsed && root) {
        const char *canonical = json_object_to_json_string_ext(root, JSON_C_TO_STRING_PLAIN);
        parsed = canonical && strlen(canonical) == length && !memcmp(canonical, data, length);
    }
    struct json_object *ok, *outputs, *rows, *routes, *route, *stats;
    size_t bytes = (size_t)count * 4 * sizeof(int32_t);
    int32_t *scratch = malloc(bytes);
    if (!scratch) { if (root) json_object_put(root); return result; }
    if (!parsed || !field(root, "ok", json_type_boolean, &ok) || !json_object_get_boolean(ok) ||
        json_object_object_length(root) != 4 || !field(root, "outputs", json_type_object, &outputs) ||
        json_object_object_length(outputs) != 1 || !field(outputs, "y", json_type_array, &rows) ||
        json_object_array_length(rows) != count || !field(root, "routes", json_type_object, &routes) ||
        json_object_object_length(routes) != 1 || !field(routes, "y", json_type_string, &route) ||
        !field(root, "stats", json_type_object, &stats)) goto done;
    const char *placement = json_object_get_string(route);
    int placement_length = json_object_get_string_len(route);
    if (placement_length == 5 && !memcmp(placement, "local", 5)) {
        if (json_object_object_length(stats)) goto done;
        result = SHIELD_PREPASS_LOCAL;
    } else if (placement_length == 17 && !memcmp(placement, "masked-field-gemm", 17)) {
        if (json_object_object_length(stats) != 3 || !counter(stats, "exchanges", (count+63)/64, (count+63)/64) ||
            !counter(stats, "macs", (int64_t)count*128, (int64_t)count*128) || !counter(stats, "verification_failures", 0, 0)) goto done;
        result = SHIELD_PREPASS_MASKED;
    } else goto done;
    for (size_t row = 0; row < count; ++row) {
        struct json_object *values = json_object_array_get_idx(rows, row);
        if (!json_object_is_type(values, json_type_array) || json_object_array_length(values) != 4) { result = SHIELD_PREPASS_FAILED; goto done; }
        for (size_t col = 0; col < 4; ++col) {
            struct json_object *value = json_object_array_get_idx(values, col);
            if (!json_object_is_type(value, json_type_int)) { result = SHIELD_PREPASS_FAILED; goto done; }
            int64_t number = json_object_get_int64(value);
            // Exact interval of four independent centered low-byte inputs.
            // This bounds only the dot product; modular row/bias terms never leave the VM.
            int64_t low = 0, high = 0;
            for (unsigned k = 0; k < 4; k++) {
                int64_t coefficient = transform->matrix[k][col];
                low += coefficient >= 0 ? -128 * coefficient : 127 * coefficient;
                high += coefficient >= 0 ? 127 * coefficient : -128 * coefficient;
            }
            if (number < low || number > high) { result = SHIELD_PREPASS_FAILED; goto done; }
            scratch[row * 4 + col] = (int32_t)number;
        }
    }
    for (unsigned row = 0; row < count; row++) {
        for (unsigned col = 0; col < 4; col++) {
            uint32_t bits = (uint32_t)scratch[row * 4 + col] + transform->bias[col] +
                            row * transform->row[col];
            memcpy(&scratch[row * 4 + col], &bits, sizeof bits);
        }
    }
    memcpy(output, scratch, bytes);
done:
    explicit_bzero(scratch, bytes); free(scratch);
    if (root) json_object_put(root);
    return result;
}
enum shield_prepass_result shield_prepass_execute_rows(const struct shield_affine64 *candidate, const uint32_t *input, unsigned count, int32_t *output) {
    enum shield_prepass_result result = SHIELD_PREPASS_FAILED;
    if (!candidate || !input || !output || !count || count > MAX_ROWS) return result;
    const struct shield_affine64 transform = *candidate;
    if (!shield_affine64_valid(&transform)) return result;
    size_t capacity = 2048u + count * 96u;
    char *body = malloc(capacity);
    if (!body) return result;
    size_t length = request(&transform, input, count, body, capacity);
    int fd = -1; char *reply = NULL;
    if (!length) goto done;
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (fd < 0) goto done;
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    memcpy(address.sun_path, SERVICE_SOCKET, sizeof SERVICE_SOCKET);
    if (connect(fd, (struct sockaddr *)&address, sizeof address)) {
        if (errno == ENOENT || errno == ECONNREFUSED) result = SHIELD_PREPASS_UNAVAILABLE;
        goto done;
    }
    struct ucred peer; socklen_t peer_size = sizeof peer;
    if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &peer, &peer_size) || peer_size != sizeof peer || peer.uid != SERVICE_UID) goto done;
    int64_t start = now_ms(); if (start < 0) goto done;
    int64_t deadline = start + TIMEOUT_MS;
    uint32_t wire_size = htonl((uint32_t)length);
    if (!transfer(fd, &wire_size, sizeof wire_size, true, deadline) || !transfer(fd, body, length, true, deadline) ||
        !transfer(fd, &wire_size, sizeof wire_size, false, deadline)) goto done;
    length = ntohl(wire_size);
    if (!length || length > 2048u+count*32u) goto done;
    reply = malloc(length + 1); if (!reply) goto done;
    if (!transfer(fd, reply, length, false, deadline)) goto done;
    reply[length] = 0;
    result = response(&transform, reply, length, count, output);
done:
    if (fd >= 0) close(fd);
    if (reply) { explicit_bzero(reply, length); free(reply); }
    explicit_bzero(body, capacity); free(body);
    return result;
}

enum shield_prepass_result shield_prepass_execute(const uint32_t input[256], int32_t output[256]) {
    const struct shield_affine64 transform = shield_affine64_fixed();
    return shield_prepass_execute_transform(&transform, input, output);
}

enum shield_prepass_result shield_prepass_execute_transform(const struct shield_affine64 *candidate, const uint32_t input[256], int32_t output[256]) {
    return shield_prepass_execute_rows(candidate, input, 64, output);
}
