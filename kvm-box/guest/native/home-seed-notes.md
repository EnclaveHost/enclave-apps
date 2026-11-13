# native_seed_home (home-seed.h)

Status: Claude contributed the helper; the main agent reviewed it, generated
`home-defaults.h`, compiled with strict warnings and tested it in an isolated
KVM guest. The original contributor did not execute tests. Current evidence
is recorded in evidence/native-home.

## Interface and placement

- `static int native_seed_home(void)`: no arguments, environment, config,
  caller paths or tenant input. Returns 0 only when every table entry has been
  created, written and given its final owner and mode. Otherwise it returns -1
  and sets errno.
- Call it exactly once in the real native nsinit, right after
  `native_prepare_root()` returns 0 (the process is chrooted, cwd is `/`) and
  **before** any role or service starts or any readiness byte goes to fd 3.
- If it returns -1, PID 1 must exit, which tears down the namespace. Do not
  retry, do not start roles, do not send readiness. Partial contents stay
  where they are: the helper never deletes anything, and the home is an
  ephemeral tmpfs that dies with the namespace.
- A second call (or any call on a nonempty home) refuses with `ENOTEMPTY`. It
  never merges into or resets an existing home.
- It does not depend on `native-root.h`. All helpers and macros use the
  `hs_`/`HS_` prefix, and `SYS_openat2`/`STATX_ATTR_MOUNT_ROOT` are wrapped in
  `#ifndef` guards, so both headers can go in one translation unit.

## What it checks and does

1. UID and EUID are 0, PID is 1, and `uname().release` is exactly
   `6.6.87.2-enclave-kvm-box`. Otherwise `EPERM`.
2. Validates the whole table before any mutation (`EINVAL`):
   - 1..64 dirs and 1..128 files, with exactly 14 files.
   - Total size at most 1 MiB with overflow-safe summation, and exactly 15720 bytes.
   - Each path is at most 255 bytes, with 1..8 components of at most 64 bytes
     drawn from `[A-Za-z0-9._-]`. No empty, `.` or `..` components, and no
     leading, trailing or doubled `/`.
   - Directories are unique and listed parent-before-child, and each one is an
     ancestor of some file, so there are no stray empty dirs.
   - Each file's parent is a listed dir. No file path is also a dir, and no
     file path appears twice.
   - Each file has mode `0644` (or `S_IFREG|0644`) and non-NULL data, and its
     path and byte count match the measured manifest list embedded in the
     header.
3. Pins `/` with `O_PATH`. Opens `home/desktop` with openat2
   `BENEATH|NO_SYMLINKS`. It must be a real directory (no symlink in any
   component) owned 1000:1000 with mode exactly 0700, a mount root per statx
   `STATX_ATTR_MOUNT_ROOT`, and `TMPFS_MAGIC` per statfs. It must be writable
   with nosuid and nodev set (noexec is not required). It must be empty, which
   is checked with raw `getdents64` into a stack buffer so nothing is allocated.
4. All later resolution adds `NO_XDEV`. For each dir, in table order: pin the
   parent, `mkdirat(...,0700)`, open it `O_RDONLY|O_DIRECTORY|O_NOFOLLOW`, and
   check it is a dir with nlink 2, owned 0:0, on the home device. Then
   `fchown(1000,1000)`, `fchmod(0755)` and re-verify.
5. For each file: pin the parent, openat2
   `O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW` with mode 0600, and write with retry on
   EINTR and short writes. Check it is a regular file with nlink 1, owned 0:0,
   with the exact size, on the home device. Then `fchown(1000,1000)`,
   `fchmod(0644)`, re-verify, and treat a `close()` failure as an error.
6. Re-checks that the home root inode, owner and 0700 mode are unchanged.

Every descriptor it opens is `O_CLOEXEC` and is closed on every path. It does
not touch caller FDs (including fd 3/4), umask, IDs or capabilities. There is
no recursion and no heap use. Creating files and dirs in the fresh home is the
only mutation.

The empty check followed by creation is not a race defence: it is only sound
because no user or tenant process exists yet. This helper cannot safely seed
a live, mutable home and does not try to.

## Generator obligations (home-defaults.h)

- Include `<sys/types.h>`/`<stddef.h>` first (home-seed.h already does this
  before including it). Define `struct native_home_file`,
  `native_home_dirs`, `native_home_files` and `NATIVE_HOME_MANIFEST_SHA256`
  as in `context/api.h`, with both arrays nonempty.
- Expected dirs, parent-before-child (10): `.config`, `.config/gtk-3.0`,
  `.fluxbox`, `.mozilla`, `.mozilla/firefox`, `.mozilla/firefox/kvm-box`,
  `.mozilla/firefox/kvm-box/chrome`, `.themes`, `.themes/RiscBox`,
  `.themes/RiscBox/gtk-3.0`.
- The helper checks paths and lengths, not content hashes. The build must check
  each embedded blob's SHA-256 against `context/home-defaults-manifest.json` and fail on any
  mismatch. It must also check that `NATIVE_HOME_MANIFEST_SHA256` matches the
  manifest it was generated from.

## Data identity vs admission

- The manifest gives the **raw data identity**: these bytes are the 14 files
  taken from `/home/desktop` in `release-v6.11-declines-candidate desktop.cpio.xz`
  (archive sha256 `16b4…fe74d`). The identity says where the bytes came from.
  It does not say they are trusted.
- **Admission** is separate and **not implemented here**. The bytes are only
  authenticated once the measured nsinit binary that embeds them is admitted
  through the outer image/profile admission. Until then, matching hashes only
  prove the build is reproducible, not that the data is authorized. Nothing in
  this helper verifies admission, and nothing should claim it does.

## Content policy

- Keep every current Fluxbox menu entry (`.fluxbox/menu`, 385 bytes) exactly as
  measured. An entry can be removed (for example for a role or feature not yet
  provided natively) only after the compute/role it refers to has been
  accepted, and the removal has to go through a new measured manifest and a
  regenerated header, never a runtime edit.
- Firefox tuning, Fluxbox, shell and GTK defaults are copied as-is. There are
  no per-tenant or runtime substitutions.

## No persistence

`/home/desktop` is a fresh 512m tmpfs created by `native_prepare_root`. Seeded
defaults and any user changes disappear when the namespace ends. Nothing here
provides or implies persistence, backup or migration of user data, and each
start re-seeds from the embedded defaults.

## VM tests to run (on the exact 6.6.87.2-enclave-kvm-box guest)

Positive:
- Normal nsinit boot: returns 0 before any role starts. `find /home/desktop`
  lists exactly 10 dirs and 14 files. Dirs are 1000:1000 0755, files are
  1000:1000 0644, nlink 1. The home root stays 1000:1000 0700. Total file bytes
  are 15745, and every file's sha256 matches `home-defaults-manifest.json`.
- Readiness on fd 3 only after a successful seed. fd 3/4 are still open and
  unchanged afterwards (`/proc/1/fdinfo`). No extra FDs are leaked
  (`ls /proc/1/fd` before and after). umask and credentials are unchanged.
- Desktop session works: Fluxbox menu shows all current entries, Firefox uses
  the `kvm-box` profile with `user.js`, and the GTK theme is RiscBox.
- Namespace restart re-seeds a clean home, and earlier user changes are gone
  (confirms the ephemeral design).

Negative (each must return -1 with the stated errno, and nsinit must exit
without starting roles or sending readiness):
- Call it twice, or pre-create any entry in the home → `ENOTEMPTY`, with no
  changes made.
- Run as non-PID-1, non-root UID/EUID, or on another kernel release → `EPERM`.
- Home not a mount root (tmpfs mount skipped), not tmpfs, mounted read-only,
  missing nosuid or nodev, wrong owner, or mode ≠ 0700 → `EPERM`.
- `/home` or `/home/desktop` is a symlink → `ELOOP`/`EXDEV` from openat2.
- Test builds with a corrupted `home-defaults.h`: a path with `..`, a leading
  `/`, an empty component, an over-long component or path, depth 9, a child
  dir listed before its parent, a duplicate path, a file whose parent is not
  listed, an unlisted extra dir, a file not in the manifest, a size off by one,
  mode 0755, NULL data, 15 files or a total ≠ 15745 → `EINVAL` before any
  mutation (home still empty).
- Fault injection (seccomp/ptrace test harness): EINTR and 1-byte short writes
  still give a byte-identical result. ENOSPC partway through (a small tmpfs)
  returns -1 and leaves only bounded partial contents. Nothing is deleted
  outside or inside the home.


## Executed qualification

Eight build-input tests pass (changed/truncated files, private state, symlinks,
hardlinks/FIFO, missing files, unsafe paths and limits, deterministic output).
The KVM probe refuses nonempty, wrong-owner, symlinked and read-only homes;
it verifies exact 14-file/15745-byte content hashes, ownership and modes as
UID1000, and refuses a repeat seed without overwriting files. Runtime cookie
creation and authenticated applications also pass in that composed probe.
The source manifest pins the pristine v6.11 home, including both compute menu
entries; these are retained until the requested compute acceptance gate.
No browser history, user passwords or Sunshine pairing state are imported.

To rebuild the data header, run build_home_defaults.py with --root pointing to
the pristine extracted image home and --output home-defaults.h. The default
manifest is adjacent to the script. Generated bytes reproduce deterministically.
Any intentional default-file update must update the pinned manifest and this
helper's fixed expected path/length table together, then rerun parity tests.

## Firefox W^X compatibility (2026-10-26)

The native and regular desktop profile defaults now set
`javascript.options.content_process_write_protect_code=true`. This retains the
JavaScript JIT while preventing simultaneous writable/executable code mappings.
The original RISC provenance remains historical; this is a deliberate native
compatibility change. The native manifest, generated header and seed byte counts
must advance together. A running profile can receive this preference and restart
Firefox; new desktop sessions require the updated measured image to retain it.
