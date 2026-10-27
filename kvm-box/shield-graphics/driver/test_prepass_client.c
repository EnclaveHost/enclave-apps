#define LEGACY_TEST_CAPACITY 16384
/* Parser/serialization tests use synthetic replies, never claim GPU work. */
#include "shield_prepass_client.c"
#include <assert.h>
static const struct shield_affine64 fixed={.matrix={{1,-1,0,0},{1,1,0,0},{0,0,1,0},{0,0,0,1}}};
static struct json_object *fixture(const char *route) {
    struct json_object *root = json_object_new_object(), *outputs = json_object_new_object(), *rows = json_object_new_array(), *routes = json_object_new_object(), *stats = json_object_new_object();
    json_object_object_add(root, "ok", json_object_new_boolean(true));
    json_object_object_add(root, "outputs", outputs); json_object_object_add(outputs, "y", rows);
    for (int row = 0; row < 64; ++row) {
        struct json_object *values = json_object_new_array();
        for (int col = 0; col < 4; ++col) json_object_array_add(values, json_object_new_int(row));
        json_object_array_add(rows, values);
    }
    json_object_object_add(root, "routes", routes); json_object_object_add(routes, "y", json_object_new_string(route));
    json_object_object_add(root, "stats", stats);
    if (!strcmp(route, "masked-field-gemm")) {
        json_object_object_add(stats, "exchanges", json_object_new_int(1));
        json_object_object_add(stats, "macs", json_object_new_int(8192));
        json_object_object_add(stats, "verification_failures", json_object_new_int(0));
    }
    return root;
}
static void run(struct json_object *root, enum shield_prepass_result expected) {
    const char *text = json_object_to_json_string_ext(root, JSON_C_TO_STRING_PLAIN);
    int32_t output[256]; memset(output, 0xa5, sizeof output);
    assert(response(&fixed,text,strlen(text),64,output) == expected);
    if (expected == SHIELD_PREPASS_FAILED) {
        for (size_t i = 0; i < sizeof output; ++i) assert(((unsigned char *)output)[i] == 0xa5);
    } else {
        for (unsigned i = 0; i < 256; ++i) assert(output[i] == (int32_t)(i / 4));
    }
    json_object_put(root);
}
int main(void) {
    char body[LEGACY_TEST_CAPACITY]; uint32_t input[256];
    for (unsigned i = 0; i < 256; ++i) input[i] = 0xffffff00u | i;
    size_t length = request(&fixed,input,64,body,LEGACY_TEST_CAPACITY); assert(length && length < LEGACY_TEST_CAPACITY);
    struct json_object *req = json_tokener_parse(body); assert(req);
    struct json_object *inputs, *rows;
    assert(field(req, "inputs", json_type_object, &inputs) && field(inputs, "x", json_type_array, &rows));
    assert(json_object_array_length(rows) == 64);
    for (unsigned row = 0; row < 64; ++row) {
        struct json_object *values = json_object_array_get_idx(rows, row); assert(json_object_array_length(values) == 32);
        for (unsigned col = 0; col < 32; ++col) assert(json_object_get_int(json_object_array_get_idx(values, col)) == (col < 4 ? (int)(row * 4 + col) - 128 : 0));
    }
    json_object_put(req);
    run(fixture("local"), SHIELD_PREPASS_LOCAL); run(fixture("masked-field-gemm"), SHIELD_PREPASS_MASKED);
    run(fixture("raw-gpu"), SHIELD_PREPASS_FAILED);
    struct json_object *root, *object, *values;
    root = fixture("masked-field-gemm"); assert(field(root, "stats", json_type_object, &object)); json_object_object_add(object, "verification_failures", json_object_new_int(1)); run(root, SHIELD_PREPASS_FAILED);
    root = fixture("masked-field-gemm"); assert(field(root, "stats", json_type_object, &object)); json_object_object_add(object, "exchanges", json_object_new_int(0)); run(root, SHIELD_PREPASS_FAILED);
    for (const char **name = (const char *[]){"exchanges", "macs", NULL}; *name; ++name) {
        root = fixture("masked-field-gemm"); assert(field(root, "stats", json_type_object, &object));
        json_object_object_add(object, *name, json_object_new_int(!strcmp(*name, "macs") ? 8193 : 2)); run(root, SHIELD_PREPASS_FAILED);
    }
    root = fixture("local"); json_object_object_add(root, "ok", json_object_new_int(1)); run(root, SHIELD_PREPASS_FAILED);
    root = fixture("local"); json_object_object_add(root, "extra", json_object_new_int(0)); run(root, SHIELD_PREPASS_FAILED);
    for (int kind = 0; kind < 4; ++kind) {
        root = fixture("local"); assert(field(root, "outputs", json_type_object, &object)); assert(field(object, "y", json_type_array, &rows)); values = json_object_array_get_idx(rows, 63);
        struct json_object *bad = kind == 0 ? json_object_new_double(1) : kind == 1 ? json_object_new_boolean(true) : kind == 2 ? json_object_new_int64(INT64_MAX) : json_object_new_int(-129);
        json_object_array_put_idx(values, 3, bad); run(root, SHIELD_PREPASS_FAILED);
    }
    root = fixture("local"); assert(field(root, "outputs", json_type_object, &object)); assert(field(object, "y", json_type_array, &rows)); json_object_array_del_idx(rows, 63, 1); run(root, SHIELD_PREPASS_FAILED);
    root = fixture("local"); const char *valid = json_object_to_json_string_ext(root, JSON_C_TO_STRING_PLAIN); length = strlen(valid);
    char duplicate[LEGACY_TEST_CAPACITY]; int n = snprintf(duplicate, sizeof duplicate, "{\"ok\":false,%s", valid + 1); assert(n > 0);
    int32_t output[256]; memset(output, 0xa5, sizeof output);
    assert(response(&fixed,duplicate,(size_t)n,64,output) == SHIELD_PREPASS_FAILED);
    for (size_t cut = 0; cut < length; ++cut) assert(response(&fixed,valid,cut,64,output) == SHIELD_PREPASS_FAILED);
    for (size_t i = 0; i < sizeof output; ++i) assert(((unsigned char *)output)[i] == 0xa5);
    json_object_put(root);
    puts("prepass client: request, route, bounds, types, late failure, duplicate keys and every truncation passed; synthetic replies only");
}
