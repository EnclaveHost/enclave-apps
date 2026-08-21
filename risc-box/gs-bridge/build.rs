// Build the vendored ENet (Moonlight's fork). GameStream's RTSP and control
// channels ride on ENet, and the client is moonlight-common-c linked against
// this exact fork — so we link the same C rather than reimplement the wire
// format and hope it matches.
fn main() {
    let mut b = cc::Build::new();
    b.include("vendor/enet/include")
        .file("vendor/enet/callbacks.c")
        .file("vendor/enet/compress.c")
        .file("vendor/enet/host.c")
        .file("vendor/enet/list.c")
        .file("vendor/enet/packet.c")
        .file("vendor/enet/peer.c")
        .file("vendor/enet/protocol.c")
        .warnings(false);

    if cfg!(unix) {
        b.file("vendor/enet/unix.c")
            .define("HAS_POLL", None)
            .define("HAS_FCNTL", None)
            .define("HAS_INET_PTON", None)
            .define("HAS_INET_NTOP", None)
            .define("HAS_MSGHDR_FLAGS", None)
            .define("HAS_SOCKLEN_T", None);
    } else {
        b.file("vendor/enet/win32.c");
    }

    b.compile("enet");

    // libopus encodes the GameStream audio channel (src/opus.rs). Linked from
    // the system rather than vendored: it is the reference encoder, and the
    // client will not decode anything else.
    println!("cargo:rustc-link-lib=opus");

    println!("cargo:rerun-if-changed=vendor/enet");
}
