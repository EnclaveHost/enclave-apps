/* SPDX-License-Identifier: GPL-3.0-or-later
 * Enclave Shield logical GPU monitoring for nvtop. No GPU/worker access.
 */
#include "nvtop/extract_gpuinfo_common.h"
#include <arpa/inet.h>
#include <errno.h>
#include <json-c/json.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static struct gpu_info *shield_device;
extern struct gpu_vendor gpu_vendor_shield;

static int64_t millis(void) {
  struct timespec ts;
  if (clock_gettime(CLOCK_MONOTONIC, &ts)) return -1;
  return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static bool transfer(int fd, void *buf, size_t size, bool writing, int64_t deadline) {
  unsigned char *ptr = buf;
  while (size) {
    int64_t now = millis();
    if (now < 0 || now >= deadline) return false;
    struct pollfd pollfd = {.fd = fd, .events = writing ? POLLOUT : POLLIN};
    int ready = poll(&pollfd, 1, (int)(deadline - now));
    if (ready < 0 && errno == EINTR) continue;
    if (ready <= 0) return false;
    ssize_t count = writing ? send(fd, ptr, size, MSG_NOSIGNAL) : recv(fd, ptr, size, 0);
    if (count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
    if (count <= 0) return false;
    ptr += count;
    size -= (size_t)count;
  }
  return true;
}

static bool string_equals(struct json_object *obj, const char *key, const char *value) {
  struct json_object *field;
  return json_object_object_get_ex(obj, key, &field) && json_object_is_type(field, json_type_string)
      && json_object_get_string_len(field) == (int)strlen(value)
      && !memcmp(json_object_get_string(field), value, strlen(value));
}

static bool counter(struct json_object *obj, const char *key, uint64_t *out) {
  struct json_object *field;
  if (!json_object_object_get_ex(obj, key, &field) || !json_object_is_type(field, json_type_int)
      || json_object_get_int64(field) < 0 || json_object_get_uint64(field) > INT64_MAX) return false;
  *out = json_object_get_uint64(field);
  return true;
}

static bool read_status(struct gpuinfo_dynamic_info *info) {
  bool ok = false;
  int64_t start = millis();
  if (start < 0) return false;
  int64_t deadline = start + 750;
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
  if (fd < 0) return false;
  struct sockaddr_un addr = {.sun_family = AF_UNIX};
  snprintf(addr.sun_path, sizeof(addr.sun_path), "/run/shield/compute.sock");
  if (connect(fd, (struct sockaddr *)&addr, sizeof(addr))) goto done;
  // Only the measured service UID may supply telemetry. No configurable
  // endpoint, external helper, environment override, or physical GPU handle.
  struct ucred peer;
  socklen_t peer_size = sizeof(peer);
  if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &peer, &peer_size) || peer.uid != 1001 || peer.pid <= 0) goto done;
  char request[] = "{\"op\":\"status\"}";
  uint32_t size = htonl(sizeof(request) - 1);
  if (!transfer(fd, &size, sizeof(size), true, deadline)
      || !transfer(fd, request, sizeof(request) - 1, true, deadline)
      || !transfer(fd, &size, sizeof(size), false, deadline)) goto done;
  size = ntohl(size);
  if (!size || size > 4096) goto done;
  char body[4097];
  if (!transfer(fd, body, size, false, deadline)) goto done;
  body[size] = 0;
  struct json_tokener *tok = json_tokener_new_ex(12);
  if (!tok) goto done;
  json_tokener_set_flags(tok, JSON_TOKENER_STRICT | JSON_TOKENER_VALIDATE_UTF8);
  struct json_object *root = json_tokener_parse_ex(tok, body, size);
  bool parsed = json_tokener_get_error(tok) == json_tokener_success && json_tokener_get_parse_end(tok) == size;
  json_tokener_free(tok);
  struct json_object *success, *telemetry;
  if (!parsed || !json_object_is_type(root, json_type_object)
      || !json_object_object_get_ex(root, "ok", &success) || !json_object_is_type(success, json_type_boolean)
      || !json_object_get_boolean(success) || !string_equals(root, "schema", "shield-guest-status/1")
      || !string_equals(root, "gpuRoute", "masked-verified-only")
      || !json_object_object_get_ex(root, "telemetry", &telemetry) || !json_object_is_type(telemetry, json_type_object)) goto json_done;
  const char *state = NULL;
  if (string_equals(root, "state", "ready")) state = "ready";
  else if (string_equals(root, "state", "busy")) state = "busy";
  else if (string_equals(root, "state", "retired")) state = "retired";
  if (!state || !counter(root, "completed", &info->shield_jobs)
      || !counter(telemetry, "verifiedOffloads", &info->shield_exchanges)
      || !counter(telemetry, "maskedMacs", &info->shield_macs)) goto json_done;
  snprintf(info->shield_state, sizeof(info->shield_state), "%s", state);
  info->shield_valid = true;
  ok = true;
json_done:
  json_object_put(root);
done:
  close(fd);
  return ok;
}

static bool shield_init(void) {
  struct gpuinfo_dynamic_info info = {0};
  return read_status(&info);
}

static void shield_shutdown(void) { free(shield_device); shield_device = NULL; }
static const char *shield_error(void) { return "Shield telemetry unavailable"; }

static bool shield_handles(struct list_head *devices, unsigned *count) {
  *count = 0;
  shield_device = calloc(1, sizeof(*shield_device));
  if (!shield_device) return false;
  shield_device->vendor = &gpu_vendor_shield;
  snprintf(shield_device->pdev, sizeof(shield_device->pdev), "shield:0");
  list_add_tail(&shield_device->list, devices);
  *count = 1;
  return true;
}

static void shield_static(struct gpu_info *gpu) {
  snprintf(gpu->static_info.device_name, sizeof(gpu->static_info.device_name), "Enclave Shield Virtual GPU");
  SET_VALID(gpuinfo_device_name_valid, gpu->static_info.valid);
  snprintf(gpu->static_info.device_architecture, sizeof(gpu->static_info.device_architecture), "Masked offload + trusted CPU");
  SET_VALID(gpuinfo_device_architecture_valid, gpu->static_info.valid);
}

static void shield_dynamic(struct gpu_info *gpu) {
  struct gpuinfo_dynamic_info fresh = {0};
  if (!read_status(&fresh)) {
    memset(&fresh, 0, sizeof(fresh));
    snprintf(fresh.shield_state, sizeof(fresh.shield_state), "unavailable");
  }
  fresh.shield_device = true;
  // Do not synthesize utilization, VRAM residency, clocks, power, temperature
  // or process attribution from a job count or from the untrusted worker.
  gpu->dynamic_info = fresh;
}

static void shield_processes(struct gpu_info *gpu) { gpu->processes_count = 0; }

struct gpu_vendor gpu_vendor_shield = {
  .init = shield_init, .shutdown = shield_shutdown, .last_error_string = shield_error,
  .get_device_handles = shield_handles, .populate_static_info = shield_static,
  .refresh_dynamic_info = shield_dynamic, .refresh_running_processes = shield_processes,
  .name = "Enclave Shield",
};

__attribute__((constructor)) static void init_shield_vendor(void) { register_gpu_vendor(&gpu_vendor_shield); }
