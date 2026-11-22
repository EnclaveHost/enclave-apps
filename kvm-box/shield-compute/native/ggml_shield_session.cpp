// A ggml accelerator that sends weight x activation products to the Shield
// compute service over a Q8 session, as the OpenCL route does, without OpenCL.
//
// EXPERIMENT, not released. For applications built on ggml (llama.cpp and the
// like) the OpenCL route pays for a second runtime between the application and
// the session: kernel launches to recognise, buffer commands through the CPU
// runtime, a copy of every weight in device buffers. This backend is what the
// native Shield backend is to ggml, an accelerator on host memory that takes the
// matrix products and nothing else, except that the products go to the
// executor's session instead of to a worker. The application still holds no
// worker route and no pad; the executor is the boundary it was.
//
// What a session needs to name a tensor is its policy name and SHA-256. The
// name is the GGUF tensor name llama gives the weight; the digest is computed
// here from the mapped bytes, once, when the scheduler first asks whether the
// product can be taken. A tensor the service refuses is left to the CPU.
#include "ggml-backend-impl.h"
#include "ggml-backend.h"
#include "ggml-cpu.h"
#include "ggml-impl.h"
#include "ggml.h"

#include <openssl/evp.h>
#include <arpa/inet.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <atomic>
#include <cerrno>
#include <immintrin.h>
#include <sched.h>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

constexpr size_t kRegion = 64u << 20;
constexpr uint32_t kOpen = 1, kShare = 3, kMatmulShared = 4, kMailFrame = 5, kWake = 6, kNoPair = 0xffffffff;
// The executor's shared-memory request slot (its Mail), when it offers one.
struct Mail {
    std::atomic<uint32_t> request, reply, sleeping;
    uint32_t id, m, pair;
    uint32_t status, paired;
    uint64_t exchanges, macs;
};
constexpr int64_t kMaxRows = 64;

struct Frame {
    uint32_t type, id, a, b;
};

[[noreturn]] void fail(const char * why) {
    std::fprintf(stderr, "[shield-session] %s\n", why);
    std::abort();
}

void send_all(int fd, const void * data, size_t n) {
    const auto * p = static_cast<const char *>(data);
    while (n) {
        const ssize_t sent = send(fd, p, n, MSG_NOSIGNAL);
        if (sent < 0 && errno == EINTR) continue;
        if (sent <= 0) fail("session send failed");
        p += sent;
        n -= (size_t)sent;
    }
}

void receive_all(int fd, void * data, size_t n) {
    auto * p = static_cast<char *>(data);
    while (n) {
        const ssize_t got = recv(fd, p, n, 0);
        if (got < 0 && errno == EINTR) continue;
        if (got <= 0) fail("session closed");
        p += got;
        n -= (size_t)got;
    }
}

struct Session {
    std::mutex mutex;
    int fd = -1;
    unsigned char * shared = nullptr;
    // weight data -> slot, or kNoPair when the service does not take it
    std::unordered_map<const void *, uint32_t> slots;
    uint32_t next = 0;
    uint64_t requests = 0, pairs = 0;

    void connect_once() {
        if (fd >= 0) return;
        const char * engine = std::getenv("SHIELD_SESSION_ENGINE");
        const std::string request = std::string("{\"op\":\"q8_session\",\"engine\":\"") + (engine ? engine : "llama") +
                                    "\",\"arithmetic\":\"shield-fixed-point-v1\"}";
        for (int attempt = 0;; ++attempt) {
            const int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
            if (s < 0) fail("socket failed");
            sockaddr_un address{};
            address.sun_family = AF_UNIX;
            std::strcpy(address.sun_path, "/run/shield/compute.sock");
            if (connect(s, reinterpret_cast<sockaddr *>(&address), sizeof address)) fail("Shield service unavailable");
            uint32_t size = htonl((uint32_t)request.size());
            send_all(s, &size, sizeof size);
            send_all(s, request.data(), request.size());
            receive_all(s, &size, sizeof size);
            size = ntohl(size);
            if (!size || size > 4096) fail("response limit");
            std::string response(size, '\0');
            receive_all(s, response.data(), size);
            if (response.find("\"ok\": true") != std::string::npos || response.find("\"ok\":true") != std::string::npos) {
                fd = s;
                break;
            }
            close(s);
            if (response.find("compute busy") == std::string::npos || attempt > 12000) fail("Shield computation refused");
            usleep(100000);
        }
        const Frame share{kShare, 0, 0, 0};
        send_all(fd, &share, sizeof share);
        Frame reply{};
        char control[CMSG_SPACE(sizeof(int))];
        iovec part{&reply, sizeof reply};
        msghdr message{};
        message.msg_iov = &part;
        message.msg_iovlen = 1;
        message.msg_control = control;
        message.msg_controllen = sizeof control;
        ssize_t received;
        do received = recvmsg(fd, &message, MSG_CMSG_CLOEXEC | MSG_WAITALL);
        while (received < 0 && errno == EINTR);
        int shared_fd = -1;
        for (cmsghdr * c = CMSG_FIRSTHDR(&message); received > 0 && c; c = CMSG_NXTHDR(&message, c))
            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)))
                std::memcpy(&shared_fd, CMSG_DATA(c), sizeof(int));
        if (received != (ssize_t)sizeof reply || reply.type != kShare || reply.a != 0 || reply.b != kRegion || shared_fd < 0)
            fail("the service gave no shared memory");
        void * base = mmap(nullptr, 3 * kRegion, PROT_READ | PROT_WRITE, MAP_SHARED, shared_fd, 0);
        close(shared_fd);
        if (base == MAP_FAILED) fail("shared memory could not be mapped");
        shared = static_cast<unsigned char *>(base);
        if (std::getenv("SHIELD_SESSION_MAILBOX")) {
            const Frame ask{kMailFrame, 0, 0, 0};
            send_all(fd, &ask, sizeof ask);
            Frame answer{};
            receive_all(fd, &answer, sizeof answer);
            if (answer.type == kMailFrame && answer.a == 0)
                mail = reinterpret_cast<Mail *>(shared + 3 * kRegion - sizeof(Mail));
        }
    }
    Mail * mail = nullptr;
    bool pinned = false;

    // The slot for a weight, opened on first use. kNoPair when it is not the service's to take.
    uint32_t slot(const ggml_tensor * w) {
        if (const auto it = slots.find(w->data); it != slots.end()) return it->second;
        connect_once();
        unsigned char digest[EVP_MAX_MD_SIZE];
        unsigned length = 0;
        if (EVP_Digest(w->data, ggml_nbytes(w), digest, &length, EVP_sha256(), nullptr) != 1 || length != 32)
            fail("digest failed");
        struct { uint64_t k, n; char sha[64]; } body{(uint64_t)w->ne[0], (uint64_t)w->ne[1], {}};
        static const char hex[] = "0123456789abcdef";
        for (int i = 0; i < 32; ++i) {
            body.sha[2 * i] = hex[digest[i] >> 4];
            body.sha[2 * i + 1] = hex[digest[i] & 15];
        }
        const size_t name = std::strlen(w->name);
        uint32_t id = kNoPair;
        if (name && name < GGML_MAX_NAME && next < 4096) {
            const Frame open{kOpen, next, (uint32_t)name, 0};
            send_all(fd, &open, sizeof open);
            send_all(fd, &body, sizeof body);
            send_all(fd, w->name, name);
            Frame reply{};
            receive_all(fd, &reply, sizeof reply);
            if (reply.type != kOpen || reply.id != next) fail("session reply");
            if (reply.a == 0) id = next++;
        }
        slots.emplace(w->data, id);
        return id;
    }

    // rows x k activations in, rows x n products out; with `second`, its products of the same rows too.
    void products(uint32_t first, uint32_t second, int64_t rows, int64_t k, const float * x, int64_t n1, float * y1,
                  int64_t n2, float * y2) {
        std::memcpy(shared, x, (size_t)(rows * k) * 4);
        struct { Frame frame; uint64_t counters[2]; } reply{};
        if (mail) {
            if (!pinned) {
                pinned = true;
                if (const char * cpu = std::getenv("SHIELD_SESSION_CPU")) {
                    cpu_set_t set;
                    CPU_ZERO(&set);
                    CPU_SET(std::atoi(cpu), &set);
                    sched_setaffinity(0, sizeof set, &set);  // the calling thread
                }
            }
            mail->id = first;
            mail->m = (uint32_t)rows;
            mail->pair = second;
            const uint32_t mine = mail->request.load(std::memory_order_relaxed) + 1;
            mail->request.store(mine, std::memory_order_seq_cst);
            if (mail->sleeping.load(std::memory_order_seq_cst)) {
                const Frame wake{kWake, 0, 0, 0};
                send_all(fd, &wake, sizeof wake);
            }
            while (mail->reply.load(std::memory_order_acquire) != mine) _mm_pause();
            reply.frame = {kMatmulShared, first, mail->status, mail->paired};
        } else {
            const struct { Frame frame; uint32_t pair, reserved; } request{{kMatmulShared, first, (uint32_t)rows, 0}, second, 0};
            send_all(fd, &request, sizeof request);
            receive_all(fd, &reply, sizeof reply);
        }
        if (reply.frame.type != kMatmulShared || reply.frame.id != first || reply.frame.a != 0) fail("product refused");
        std::memcpy(y1, shared + kRegion, (size_t)(rows * n1) * 4);
        ++requests;
        if (second != kNoPair) {
            if (!(reply.frame.b & 1)) fail("paired product missing");
            std::memcpy(y2, shared + 2 * kRegion, (size_t)(rows * n2) * 4);
            ++pairs;
        }
    }
};

Session & session() {
    static Session * s = new Session;
    return *s;
}

bool is_product(const ggml_tensor * op) {
    if (op->op != GGML_OP_MUL_MAT) return false;
    const ggml_tensor * w = op->src[0], * x = op->src[1];
    return w && x && w->data && ggml_n_dims(w) == 2 && ggml_is_contiguous(w) && ggml_is_contiguous(x) &&
           !w->view_src && w->buffer && ggml_backend_buffer_get_usage(w->buffer) == GGML_BACKEND_BUFFER_USAGE_WEIGHTS &&
           x->type == GGML_TYPE_F32 && op->type == GGML_TYPE_F32 && x->ne[0] == w->ne[0] && w->ne[0] % 32 == 0 &&
           x->ne[2] == 1 && x->ne[3] == 1 && w->ne[0] * w->ne[1] >= (1 << 20) && w->name[0];
}

const char * backend_name(ggml_backend_t) { return "ShieldSession"; }

void backend_free(ggml_backend_t backend) { delete backend; }

ggml_status graph_compute(ggml_backend_t, ggml_cgraph * graph) {
    Session & s = session();
    std::lock_guard lock(s.mutex);
    std::vector<char> done((size_t)graph->n_nodes, 0);
    for (int i = 0; i < graph->n_nodes; ++i) {
        ggml_tensor * node = graph->nodes[i];
        if (done[(size_t)i] || node->op != GGML_OP_MUL_MAT) continue;
        const ggml_tensor * w = node->src[0], * x = node->src[1];
        const uint32_t first = s.slot(w);
        if (first == kNoPair) fail("a product reached the session backend for a tensor the service refused");
        // The next product of the same activation rides along.
        int with = -1;
        uint32_t second = kNoPair;
        for (int j = i + 1; j < graph->n_nodes && j < i + 8; ++j) {
            const ggml_tensor * other = graph->nodes[j];
            if (!done[(size_t)j] && other->op == GGML_OP_MUL_MAT && other->src[1] == x && other->src[0] != w) {
                const uint32_t id = s.slot(other->src[0]);
                if (id != kNoPair) {
                    with = j;
                    second = id;
                }
                break;
            }
        }
        const int64_t k = w->ne[0], n1 = w->ne[1], rows = x->ne[1];
        const int64_t n2 = with >= 0 ? graph->nodes[with]->src[0]->ne[1] : 0;
        const int64_t widest = std::max(n1, n2);
        const int64_t step = std::max<int64_t>(1, std::min<int64_t>(kMaxRows, (int64_t)(kRegion / 4) / std::max(widest, k)));
        for (int64_t at = 0; at < rows; at += step) {
            const int64_t take = std::min(step, rows - at);
            s.products(first, second, take, k, static_cast<const float *>(x->data) + at * k, n1,
                       static_cast<float *>(node->data) + at * n1, n2,
                       with >= 0 ? static_cast<float *>(graph->nodes[with]->data) + at * n2 : nullptr);
        }
        done[(size_t)i] = 1;
        if (with >= 0) done[(size_t)with] = 1;
    }
    return GGML_STATUS_SUCCESS;
}

ggml_backend_i backend_interface = {
    /* .get_name                = */ backend_name,
    /* .free                    = */ backend_free,
    /* .set_tensor_async        = */ nullptr,
    /* .get_tensor_async        = */ nullptr,
    /* .set_tensor_2d_async     = */ nullptr,
    /* .get_tensor_2d_async     = */ nullptr,
    /* .cpy_tensor_async        = */ nullptr,
    /* .synchronize             = */ nullptr,
    /* .graph_plan_create       = */ nullptr,
    /* .graph_plan_free         = */ nullptr,
    /* .graph_plan_update       = */ nullptr,
    /* .graph_plan_compute      = */ nullptr,
    /* .graph_compute           = */ graph_compute,
    /* .event_record            = */ nullptr,
    /* .event_wait              = */ nullptr,
    /* .graph_optimize          = */ nullptr,
};

ggml_guid_t backend_guid() {
    static ggml_guid guid = {0x53, 0x68, 0x69, 0x65, 0x6c, 0x64, 0x53, 0x65, 0x73, 0x73, 0x69, 0x6f, 0x6e, 0x51, 0x38, 0x01};
    return &guid;
}

const char * device_name(ggml_backend_dev_t) { return "ShieldSession"; }
const char * device_description(ggml_backend_dev_t) { return "Enclave Shield products over a service session"; }
void device_memory(ggml_backend_dev_t, size_t * free, size_t * total) { *free = 0; *total = 0; }
enum ggml_backend_dev_type device_type(ggml_backend_dev_t) { return GGML_BACKEND_DEVICE_TYPE_ACCEL; }
void device_props(ggml_backend_dev_t dev, ggml_backend_dev_props * props) {
    props->name = device_name(dev);
    props->description = device_description(dev);
    props->type = device_type(dev);
    device_memory(dev, &props->memory_free, &props->memory_total);
    props->caps = {
        /* .async                 = */ false,
        /* .host_buffer           = */ false,
        /* .buffer_from_host_ptr  = */ true,
        /* .events                = */ false,
    };
}
ggml_backend_t device_init(ggml_backend_dev_t dev, const char *) {
    return new ggml_backend{
        /* .guid    = */ backend_guid(),
        /* .iface   = */ backend_interface,
        /* .device  = */ dev,
        /* .context = */ nullptr,
    };
}
ggml_backend_buffer_type_t device_buffer_type(ggml_backend_dev_t) { return ggml_backend_cpu_buffer_type(); }
ggml_backend_buffer_t device_buffer_from_host_ptr(ggml_backend_dev_t, void * ptr, size_t size, size_t) {
    return ggml_backend_cpu_buffer_from_ptr(ptr, size);
}
bool device_supports_op(ggml_backend_dev_t, const ggml_tensor * op) {
    switch (op->op) {
    case GGML_OP_NONE:
    case GGML_OP_RESHAPE:
    case GGML_OP_VIEW:
    case GGML_OP_PERMUTE:
    case GGML_OP_TRANSPOSE:
        return true;
    case GGML_OP_MUL_MAT: {
        if (!is_product(op)) return false;
        Session & s = session();
        std::lock_guard lock(s.mutex);
        return s.slot(op->src[0]) != kNoPair;
    }
    default:
        return false;
    }
}
bool device_supports_buft(ggml_backend_dev_t, ggml_backend_buffer_type_t buft) { return ggml_backend_buft_is_host(buft); }

const ggml_backend_device_i device_interface = {
    /* .get_name             = */ device_name,
    /* .get_description      = */ device_description,
    /* .get_memory           = */ device_memory,
    /* .get_type             = */ device_type,
    /* .get_props            = */ device_props,
    /* .init_backend         = */ device_init,
    /* .get_buffer_type      = */ device_buffer_type,
    /* .get_host_buffer_type = */ nullptr,
    /* .buffer_from_host_ptr = */ device_buffer_from_host_ptr,
    /* .supports_op          = */ device_supports_op,
    /* .supports_buft        = */ device_supports_buft,
    /* .offload_op           = */ nullptr,
    /* .event_new            = */ nullptr,
    /* .event_free           = */ nullptr,
    /* .event_synchronize    = */ nullptr,
};

const char * reg_name(ggml_backend_reg_t) { return "ShieldSession"; }
size_t reg_device_count(ggml_backend_reg_t) { return 1; }
ggml_backend_dev_t reg_device(ggml_backend_reg_t reg, size_t index) {
    GGML_ASSERT(index == 0);
    static ggml_backend_device device = {
        /* .iface   = */ device_interface,
        /* .reg     = */ reg,
        /* .context = */ nullptr,
    };
    return &device;
}
void * reg_proc_address(ggml_backend_reg_t, const char *) { return nullptr; }

const ggml_backend_reg_i reg_interface = {
    /* .get_name         = */ reg_name,
    /* .get_device_count = */ reg_device_count,
    /* .get_device       = */ reg_device,
    /* .get_proc_address = */ reg_proc_address,
};

}  // namespace

ggml_backend_reg_t ggml_backend_shield_session_reg(void) {
    static ggml_backend_reg reg = {
        /* .api_version = */ GGML_BACKEND_API_VERSION,
        /* .iface       = */ reg_interface,
        /* .context     = */ nullptr,
    };
    return &reg;
}

GGML_BACKEND_DL_IMPL(ggml_backend_shield_session_reg)
