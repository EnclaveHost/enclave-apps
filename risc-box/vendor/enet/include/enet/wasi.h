/**
 @file  wasi.h
 @brief ENet platform header for wasm32-wasip2.

 Replaces unix.h. ENet's protocol core is portable C and is vendored verbatim,
 so the wire format matches moonlight-common-c's fork by construction — but its
 platform layer is BSD sockets and poll, and a wasm guest reaches the network
 through wasi:sockets. So the sockets live in Rust (src/gamestream/enet_sys.rs)
 and this header only has to describe the types the core passes across.

 There is no C sysroot at cargo time (see build.rs), so the POSIX types the
 core needs are defined here rather than included.
*/
#ifndef __ENET_WASI_H__
#define __ENET_WASI_H__

#include <stddef.h>
#include <stdlib.h>

typedef unsigned int socklen_t;

/* Layout-compatible with what the Rust side mirrors in enet_sys.rs: the fork
   carries a sockaddr_storage inline, so the size and alignment are load
   bearing, not decorative. */
struct sockaddr
{
    unsigned short sa_family;
    char           sa_data[14];
};

struct sockaddr_storage
{
    unsigned short ss_family;
    char           __ss_pad[126];
} __attribute__ ((aligned (8)));

struct in_addr { unsigned int s_addr; };

struct sockaddr_in
{
    unsigned short sin_family;
    unsigned short sin_port;
    struct in_addr sin_addr;
    char           sin_zero[8];
};

#define AF_INET  2
#define AF_INET6 10

typedef int ENetSocket;

#define ENET_SOCKET_NULL -1

/* wasm32 is little-endian, so network order is always a byte swap. Written as
   builtins rather than htons/htonl because there is no libc header to take
   them from. */
#define ENET_HOST_TO_NET_16(value) (__builtin_bswap16 (value))
#define ENET_HOST_TO_NET_32(value) (__builtin_bswap32 (value))
#define ENET_NET_TO_HOST_16(value) (__builtin_bswap16 (value))
#define ENET_NET_TO_HOST_32(value) (__builtin_bswap32 (value))

typedef struct
{
    void * data;
    size_t dataLength;
} ENetBuffer;

#define ENET_CALLBACK
#define ENET_API extern

/* The host loop is polled from Rust and never selects, but the core still
   names the type. One socket is all this host has. */
typedef struct { int count; int fds[4]; } ENetSocketSet;

#define ENET_SOCKETSET_EMPTY(sockset)          ((sockset).count = 0)
#define ENET_SOCKETSET_ADD(sockset, socket)    ((sockset).fds[(sockset).count++] = (socket))
#define ENET_SOCKETSET_REMOVE(sockset, socket) ((void) 0)
#define ENET_SOCKETSET_CHECK(sockset, socket)  (1)

#endif /* __ENET_WASI_H__ */
