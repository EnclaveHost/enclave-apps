# native-root.h — native desktop root preparation

`static int native_prepare_root(void)` is one step of the future real nsinit.
It is **not** the full nsinit, does not admit anything and does not make the
desktop ready. Runtime admission stays disabled. Claude contributed and statically reviewed
the helper; the main agent compiled it, applied review fixes and ran real
isolated KVM layout/input tests. Exact scope and hashes are in the exported
evidence. The list below also includes follow-up tests not yet completed.

It needs only libc (glibc ≥ 2.28 for `statx`) and Linux UAPI headers
(`<linux/magic.h>`). Include it before other system headers, or build with
`-D_GNU_SOURCE`.

## Contract

- Caller: nsinit as UID 0 / PID 1, inside the supervisor's new
  PID/mount/IPC/UTS namespaces. There is no new user or network namespace.
- Input: none. There are no arguments, environment, config files, endpoints
  or tenant paths. Every path is a fixed literal.
- Success (`0`): the process has been `chroot`ed into the prepared root with
  cwd `/`. Every descriptor the helper opened is closed.
- Failure (`-1`, `errno` set): if the failure happens before `chroot`, the
  helper:
  - restores any uinput/UHID ownership it changed
  - restores the original cwd
  - lazily detaches **only** the mount on the fixed target, which it had
    first verified was not a mount point
  - closes every descriptor it opened

  The empty target directory, if created, is left in place. There is no
  recursive removal. On any `-1`, PID 1 must exit so the namespace is torn
  down; it must not retry in place.
- fd 3 (readiness pipe) and fd 4 (supervisor pidfd) are never read, written
  or closed. Other inherited descriptors are not touched. The helper writes no
  readiness byte.

## Trusted preconditions (provided by the future measured platform launcher)

The helper checks what it can observe: types, owners, modes, mount roots,
read-only flags, device numbers and filesystem magic. It **cannot prove
attestation or content authenticity**. Read-only mounts do not authenticate
their content. The following preconditions are assumed, not established, here:

1. **Exclusive desktop guest.** The VM is dedicated to this desktop, so binding
   guest `/dev/input`, uinput and UHID never exposes physical host input.
2. **Admission done.** Full image, profile and admission checks, plus
   authenticated model verification, happened before the supervisor started.
3. `/plat/native-desktop/rootfs` is a root-owned, read-only mount of the
   measured desktop image. The image already contains real directories (no
   symlink components) for `/dev /sys /proc /run /tmp /var/log /var/tmp
   /home/desktop /models`. Image preparation supplies any missing ones; this
   helper never edits the source. `/plat` and `/plat/native-desktop` are
   root-owned and not group/world writable.
4. `/run/native-desktop` is root:1002 mode 0750 (the real supervisor's
   directory). It contains:
   - `models`: a root-owned, read-only mount of verified model data
   - `worker.sock`: the live root worker broker socket, root:1001 mode 0660.
     The broker enforces peer UID 1001 and an authenticated reservation.
5. Guest `/dev` is a devtmpfs mount root containing the expected nodes, with
   these exact modes and owners:
   - null, zero, random, urandom, full, tty: root:root 0666
   - uinput, uhid: 0600, owned by root:root or, from an earlier run, 1000:1000
6. `/dev/input` exists as a root:root 0755 directory before uinput creates
   anything. The launcher creates it; this helper never mutates source `/dev`.
7. The original `/proc` is an ancestor-PID-namespace procfs mount; the supervisor must not remount it for the child before this helper. The helper requires `/proc/self` to name a PID other than 1 and a mount namespace different from the init visible through that procfs, before any propagation change. `/sys` is guest sysfs.
8. There is no CPU-only or missing-worker fallback. A missing, wrong-type or
   wrong-owner socket fails closed.

## What the helper does, in order

1. Checks `getuid()==geteuid()==0`, `getpid()==1` and the exact kernel release
   `6.6.87.2-enclave-kvm-box`. Validates the ancestor procfs and distinct mount namespace, then runs `mount(NULL,"/",MS_REC|MS_PRIVATE)`
   again as defence in depth.
2. **Pins all sources first** with `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)`,
   one fixed path each, starting from an `O_PATH` fd for `/`:
   - rootfs, models, worker socket, sysfs and `/dev/input`
   - the eight device nodes (`O_PATH`, trailing symlinks rejected), each checked
     for `S_ISCHR`, exact `rdev`, mode and owner

   The original `/proc`, the parent directories and the mount-root/RO
   properties are validated at the same time. Sources are pinned before any
   mount, so the later private `/run` cannot hide them. Every later mount
   names its source through `/proc/self/fd/N` of the original `/proc`.
3. **Target** `/run/native-desktop/root`:
   - opened with no symlink resolution
   - created as root 0700 if absent
   - must be root:root mode 0700, on the same device as its parent, not a
     mount root, and empty

   Anything else fails, including a symlink, a non-directory, an occupied
   directory, an existing mount or wrong permissions.
4. Non-recursively binds the rootfs onto the target, then remounts it
   `ro,nosuid,nodev`. The new mount root is reopened and checked to be the
   same inode as the pinned source, with the exact effective flags (`statfs`).
5. Pins all nine image mountpoints with
   `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_XDEV` before mounting
   anything inside. Every later mount targets `/proc/self/fd/N` of a pinned
   descriptor. Each result is reopened and checked to be a mount root, with
   the expected magic and exact `ro/nosuid/nodev/noexec` bits.
6. Builds the layout below.
7. Changes uinput/UHID to 1000:1000 0600 through the pinned fds and verifies
   the change. This is the last step before chroot.
8. `fchdir(newroot)`, `chroot(".")`, `chdir("/")`, then closes all its fds.

## Resulting layout

| Path | Mount | Options |
|---|---|---|
| `/` | bind of rootfs (non-recursive) | ro,nosuid,nodev |
| `/proc` | new procfs (this PID ns) | nosuid,nodev,noexec,hidepid=2 |
| `/proc/{sys,sysrq-trigger,bus,irq}` | individual self-binds | ro,nosuid,nodev,noexec |
| `/sys` | bind of guest sysfs (non-recursive) | ro,nosuid,nodev,noexec |
| `/dev` | tmpfs 8 MiB 0755 | nosuid,nodev,noexec (individual device binds override nodev) |
| `/dev/{null,zero,random,urandom,full,tty}` | bind of pinned node onto regular placeholder | ro,nosuid,noexec; 0666 root kept |
| `/dev/{uinput,uhid}` | same as above | ro,nosuid,noexec; inode re-owned 1000:1000 0600 through original source |
| `/dev/input` | bind of guest dir (non-recursive), RW | nosuid,noexec |
| `/dev/pts` | devpts newinstance | mode=0600,gid=1000,ptmxmode=0666,nosuid,noexec |
| `/dev/ptmx` | symlink → `pts/ptmx` | |
| `/dev/shm` | tmpfs 256 MiB 1777 | nosuid,nodev,noexec |
| `/dev/fd`, `stdin`, `stdout`, `stderr` | symlinks → `/proc/self/fd[/0-2]` | |
| `/run` | tmpfs 64 MiB 0755 | nosuid,nodev,noexec |
| `/run/user` 0755 root, `/run/user/1000` 0700 1000:1000 | dirs | |
| `/run/udev` 0755 root | dir | |
| `/run/shield` 0750 1001:1000, `/run/shield/jobs` 0700 1001:1001 | dirs | |
| `/run/shield-worker` 0750 root:1001 | dir | |
| `/run/shield-worker/gpu0` | bind of pinned broker socket | ro,nosuid,nodev,noexec; inode root:1001 0660 |
| `/tmp` | tmpfs 256 MiB 1777 | nosuid,nodev; `/tmp/.X11-unix` root 1777 |
| `/var/log` | tmpfs 32 MiB 0755 | nosuid,nodev,noexec |
| `/var/tmp` | tmpfs 256 MiB 1777 | nosuid,nodev |
| `/home/desktop` | tmpfs 512 MiB 0700 uid/gid 1000 | nosuid,nodev — **empty, see seeding** |
| `/models` | bind of verified source (non-recursive) | ro,nosuid,nodev,noexec |

The only writable mounts are private tmpfs/devpts mounts, plus guest
`/dev/input`, which is writable for confined udev's ownership changes. The
source parent filesystem gains at most the empty target directory. All mounts
exist only in this private mount namespace.

## Security notes and explicit scope

- **Input devices are not namespace-scoped.** Binding `/dev/input` and
  re-owning uinput/UHID is safe **only** in the exclusive desktop guest. It
  must never be used with physical host input. The ownership change happens on
  the guest's shared devtmpfs inode, so it is visible guest-wide. Udev symlinks
  under `/dev/input/by-*` also land in guest devtmpfs.
- There are no DRM/GPU/TPM/KVM/vsock/disk nodes and no whole-devtmpfs bind.
  The desktop reaches the GPU only through the broker socket.
- `/proc/sys`, `sysrq-trigger`, `bus` and `irq` are read-only. UID-0 udev
  cannot write sysctls, sysrq, PCI configuration or IRQ controls through them.
  Capability drops alone do not protect all procfs write handlers.
- The eight fixed device binds are read-only for inode metadata; device I/O
  still works. Neither desktop UID nor confined udev can chmod/re-own them.
  Udev has no DAC_OVERRIDE and cannot open desktop-owned UHID. The guest input
  directory remains writable for event-node ownership changes. The pinned
  kernel's devtmpfs magic is checked and nested source device binds are refused.
- Desktop PTY slaves use mode 0600, preventing UID 1001/1003 (group 1000)
  from writing terminal output. The fixed guest `/dev/tty` (5:0) and `/dev/full` (1:7) are now exposed
  with the same read-only metadata policy. `/dev/tty` was required by the real
  xterm test. The guest console is not exposed.
- `/sys` is a non-recursive bind, so nested mounts such as cgroupfs, debugfs,
  securityfs and bpffs are not visible.
- **`chroot` is not a security boundary for privileged code.** Any process
  that keeps CAP_SYS_CHROOT or CAP_SYS_ADMIN, or holds an fd outside the root,
  can escape. Roles must drop those capabilities and never inherit outer fds.
  Replace this with `pivot_root` plus detaching the old root if stronger
  isolation of nsinit itself is needed.
- Worker socket checks cover metadata only. The helper does not connect, so a
  stale inode left by a dead broker is not detected here. The worker client
  (UID 1001) must check `SO_PEERCRED` uid 0 on the server separately, and the
  broker must still enforce peer UID 1001 and an authenticated reservation.
  A read-only bind does not block `connect()`, because socket write
  permission is checked against the inode and superblock, not the mount.
- Source descriptors are `O_CLOEXEC` and are all closed before return, so none
  reach roles.
- Possible failures that are deliberately not worked around (all fail closed):
  - an unusual umask can only fail closed. The created target is `fchmod`ed
    to 0700, and tmpfs directories are `fchmod`ed explicitly.
  - `statx` must report `STATX_ATTR_MOUNT_ROOT`, otherwise the helper returns
    `ENOTSUP`.

## Remaining nsinit obligations (not done here)

1. **Default-home seeding is mandatory.** The measured image's
   `/home/desktop` has the Firefox and Fluxbox defaults, but this tmpfs covers
   it. nsinit must copy those defaults from source data it opened or pinned
   **before** chroot, or from a fixed read-only measured location, into the
   empty home. Ownership must be 1000:1000 and no symlinks may be followed.
   **Readiness must not be sent before seeding succeeds.** An empty home is
   not a complete desktop.
2. Role launches: UID/GID and supplementary groups, removing the capability
   bounding/ambient sets, NoNewPrivs, seccomp per role (namespaces, mount,
   AF_VSOCK, io_uring, …), and closing fds 3 and 4 in every role. udev uses
   `udev-launch.c`.
3. Worker client peer verification (server UID 0) and broker reservation use.
4. Supervising services: PID-1 signal handling and reaping, monitoring fd 4,
   and handling service failure.
5. Outer cgroup CPU and memory enforcement. The tmpfs size limits are not a
   memory-accounting boundary.
6. Readiness: write exactly `R` to fd 3 and close it, only after all services
   are up and authenticated bindings exist.
7. Secret isolation: no owner credentials or secrets in the root, environment
   or inherited fds. The launcher must recreate `/run/native-desktop` state
   after a failure.

## Tests the main agent should run (real namespaces/VM; none run yet)

Positive:
1. Run through the real supervisor in the target VM. Expect a return of 0,
   then check:
   - `/proc/self/mountinfo` matches the table exactly (fstype, flags, sizes,
     modes and owners)
   - no extra mounts, and nothing under `/sys` beyond sysfs itself
2. fds 3 and 4 are unchanged (same inode via `fstat`) and `/proc/self/fd`
   lists no extra fds. No readiness byte arrives on fd 3.
3. Device behaviour:
   - reading `/dev/null`, `zero` and `urandom` works; writing `/dev/null` works
   - as UID 1000, opening `/dev/uinput` creates an event node that appears in
     `/dev/input`, and UHID works
   - `/dev/ptmx` gives a pty under `/dev/pts` with gid 1000 and mode 0620
4. As UID 1001, `connect()` to `/run/shield-worker/gpu0` reaches the broker,
   which confirms connect works through the RO bind. UID 1000 gets EACCES.
5. `/proc` shows only this PID namespace. Writing `/proc/sys/net/...` gets
   EROFS. Root-level writes to `/`, `/sys`, `/models` and the gpu0 bind get
   EROFS.
6. Writes to `/tmp`, `/var/tmp`, `/run/user/1000` and `/home/desktop` work as
   UID 1000. Exec is refused on `/run`, `/dev/shm`, `/var/log` and `/dev`.
   Setuid binaries copied to `/tmp` do not gain privilege.
7. A second run against an existing empty root 0700 target also succeeds.

Negative (each expects -1, the target mount absent afterwards, source
unchanged and no new fds):
1. Wrong kernel release, non-PID-1 caller, or non-root caller.
2. Target problems:
   - the target is a symlink (to an empty dir or to `/`)
   - the target is non-empty, a file, mode 0755, or owned by a non-root user
   - the target is already a mount point (confirm the preexisting mount is
     **not** detached)
3. Source rootfs or models mount is RW; the source is a plain directory rather
   than a mount; or `/plat/native-desktop` is a symlink or group-writable.
4. `/run/native-desktop` has the wrong group or mode; `worker.sock` is missing,
   is a regular file or symlink, or has the wrong group or mode.
5. A device node is replaced by a symlink, a node with a different rdev, or a
   regular file. A null node has the wrong mode. `/dev/input` is missing,
   group-writable, or a symlink.
6. Image mountpoint problems: `/var` is a symlink, `/home/desktop` is a
   symlink or file, or `/models` is missing.
7. Inject a failure late, for example a missing `/models` mountpoint after
   `/dev` setup. Confirm the whole target subtree is detached, uinput/UHID
   owners are restored, and cwd is restored.
8. Confirm `/sys/fs/cgroup`, DRM, KVM, vsock, TPM and disk nodes are absent
   inside the root.


## Review and qualification update

Claude's independent static review identified the procfs and device-metadata
issues above, DAC_OVERRIDE/UHID access, missing namespace checks and shared
TTY-group writes. The main agent implemented these fixes. Real KVM tests pass
with three udev capabilities (CHOWN, FOWNER, NET_ADMIN; mask 0x1009), read-only
fixed devices, nodev on the containing /dev tmpfs, and eight rejection/fault
cases including an injected chroot failure after device re-owning. That failure
restores ownership and cwd, closes source descriptors and detaches the helper
mount. Real uinput/udev/libinput/Xorg input still works. See current receipts
for exact tested versions and additional checks.

Mount operations name pinned source/target descriptors via the original
`/proc/self/fd` path; the helper does not retain a pinned procfs descriptor.
This path remains stable because the helper is single-threaded and alone in
its private mount namespace until it changes root.

Full nsinit must still enforce the inherited-FD/clean-environment contract,
seed measured desktop defaults, supervise services, and prove authenticated
model/worker admission. It must configure X authentication and address the
shared network namespace's abstract Unix sockets (including Xorg's local
listener). A measured UID-1003 static server is required for owner-origin
content. Udev remains measured trusted code, not an arbitrary-root sandbox.

The earlier suggested missing-/models failure occurs before mounting, so it
does not test late cleanup. The executed late-failure case instead uses a
test-only seccomp rule returning EPERM for chroot after all mount/device work.


Default-home seeding is now implemented by `home-seed.h`, using the compiled
`home-defaults.h` table generated from the pinned `home-defaults-manifest.json`.
Call it after root preparation and before any roles start; no source directory
FD needs to survive chroot for this path. Exact 14-file parity was verified in
the isolated KVM guest, along with authenticated Xorg, Fluxbox, xterm, Firefox,
and custom FreeDoom. Integration into full nsinit and authenticated image
admission remain mandatory. See evidence/native-home for exact scope.
