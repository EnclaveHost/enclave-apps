// vm-init.c - PID 1 of the KVM Box whole-VM guest's measured initramfs.
//
// The launch measurement covers this program, the kernel and the kernel command line. Everything else the guest runs
// comes from two disks the host serves, so nothing from them is used before dm-verity stands between it and the host:
//
//   enclave.roothash=<64 hex>                         root hash of the desktop root (squashfs)
//   enclave.root=<data blocks>:<salt hex>             its verity geometry (hash tree on the ROOT-HASH disk, no superblock)
//   enclave.models=<data blocks>:<root hash>:<salt>   optional: the model volume (ext4, its tree appended one block
//                                                     after the data, as metal/volumes.mjs builds it)
//
// Disks are found by virtio serial (ROOT-DATA, ROOT-HASH, MODELS), never by order. The root is mounted read-only under
// a tmpfs overlay: every write is guest memory and is gone at power-off. Any failure powers the guest off; there is no
// shell and no fallback path. Then /vm/start of the verified root runs as this process's child.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/dm-ioctl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

static void die(const char *fmt, ...) {
  va_list ap; va_start(ap, fmt);
  fputs("VMINIT FATAL: ", stdout); vprintf(fmt, ap); printf(" (%s)\n", strerror(errno)); fflush(stdout);
  va_end(ap);
  sync(); sleep(1); reboot(RB_POWER_OFF); _exit(1);
}

static int hexn(const char *s, size_t n) {
  if (strlen(s) != n) return 0;
  for (size_t i = 0; i < n; i++) if (!strchr("0123456789abcdef", s[i])) return 0;
  return 1;
}

// The value of key= on the kernel command line, or NULL. The returned string is owned by the caller.
static char *cmdline_value(const char *key) {
  static char line[4096];
  int fd = open("/proc/cmdline", O_RDONLY);
  if (fd < 0) die("cmdline");
  ssize_t n = read(fd, line, sizeof(line) - 1); close(fd);
  if (n <= 0) die("cmdline read");
  line[n] = 0; line[strcspn(line, "\n")] = 0;
  size_t klen = strlen(key);
  char *found = NULL, *save = NULL;
  for (char *tok = strtok_r(line, " ", &save); tok; tok = strtok_r(NULL, " ", &save))
    if (!strncmp(tok, key, klen) && tok[klen] == '=') {
      if (found) { errno = EINVAL; die("%s given twice", key); }
      found = strdup(tok + klen + 1);
    }
  return found;
}

static void load_modules(const char *list, const char *prefix) {
  FILE *f = fopen(list, "r");
  if (!f) die("module list %s", list);
  char rel[400], path[512];
  while (fgets(rel, sizeof(rel), f)) {
    rel[strcspn(rel, "\n")] = 0;
    if (!rel[0]) continue;
    snprintf(path, sizeof(path), "%s%s", prefix, rel);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) die("module %s", path);
    if (syscall(SYS_finit_module, fd, "", 0) < 0 && errno != EEXIST) die("load %s", path);
    close(fd);
  }
  fclose(f);
}

// "major:minor" of the virtio disk with this serial. Exactly one must exist.
static void disk_by_serial(const char *serial, char *out, size_t outlen, uint64_t *sectors) {
  DIR *d = opendir("/sys/block");
  if (!d) die("/sys/block");
  int hits = 0; struct dirent *e; char path[300], buf[64];
  while ((e = readdir(d))) {
    if (strncmp(e->d_name, "vd", 2)) continue;
    snprintf(path, sizeof(path), "/sys/block/%s/serial", e->d_name);
    int fd = open(path, O_RDONLY); if (fd < 0) continue;
    ssize_t n = read(fd, buf, sizeof(buf) - 1); close(fd);
    if (n < 0) continue;
    buf[n] = 0; buf[strcspn(buf, "\n")] = 0;
    if (strcmp(buf, serial)) continue;
    hits++;
    snprintf(path, sizeof(path), "/sys/block/%s/dev", e->d_name);
    fd = open(path, O_RDONLY); if (fd < 0) die("%s", path);
    n = read(fd, out, outlen - 1); close(fd);
    if (n <= 0) die("%s read", path);
    out[n] = 0; out[strcspn(out, "\n")] = 0;
    snprintf(path, sizeof(path), "/sys/block/%s/size", e->d_name);
    fd = open(path, O_RDONLY); if (fd < 0) die("%s", path);
    n = read(fd, buf, sizeof(buf) - 1); close(fd);
    if (n <= 0) die("%s read", path);
    buf[n] = 0; *sectors = strtoull(buf, NULL, 10);
  }
  closedir(d);
  if (hits != 1) { errno = ENODEV; die("%d disks with serial %s", hits, serial); }
}

// A read-only dm-verity device: sha256, 4096-byte blocks, format 1, no optional arguments, so a block that does not
// verify is an I/O error. Returns its device number.
static dev_t verity_open(const char *name, const char *data, const char *hash, uint64_t blocks, uint64_t hash_start,
                         const char *root, const char *salt) {
  int ctl = open("/dev/mapper/control", O_RDWR | O_CLOEXEC);
  if (ctl < 0) {
    mkdir("/dev/mapper", 0755);
    if (mknod("/dev/mapper/control", S_IFCHR | 0600, makedev(10, 236)) < 0 && errno != EEXIST) die("mapper control node");
    ctl = open("/dev/mapper/control", O_RDWR | O_CLOEXEC);
    if (ctl < 0) die("mapper control");
  }
  union { struct dm_ioctl io; char raw[4096]; } b;
#define DM_INIT() do { memset(&b, 0, sizeof(b)); b.io.version[0] = DM_VERSION_MAJOR; b.io.data_size = sizeof(b); \
                       b.io.data_start = sizeof(struct dm_ioctl); strncpy(b.io.name, name, sizeof(b.io.name) - 1); } while (0)
  DM_INIT();
  b.io.flags = DM_READONLY_FLAG;
  if (ioctl(ctl, DM_DEV_CREATE, &b) < 0) die("dm create %s", name);
  DM_INIT();
  b.io.flags = DM_READONLY_FLAG; b.io.target_count = 1;
  struct dm_target_spec *t = (struct dm_target_spec *)(b.raw + sizeof(struct dm_ioctl));
  t->sector_start = 0; t->length = blocks * 8; strcpy(t->target_type, "verity");
  char *params = (char *)(t + 1);
  int room = (int)(sizeof(b) - sizeof(struct dm_ioctl) - sizeof(*t));
  int n = snprintf(params, room, "1 %s %s 4096 4096 %llu %llu sha256 %s %s", data, hash,
                   (unsigned long long)blocks, (unsigned long long)hash_start, root, salt);
  if (n <= 0 || n >= room) { errno = E2BIG; die("verity table %s", name); }
  t->next = (uint32_t)((sizeof(*t) + n + 1 + 7) & ~7u);
  if (ioctl(ctl, DM_TABLE_LOAD, &b) < 0) die("dm table %s", name);
  DM_INIT();
  if (ioctl(ctl, DM_DEV_SUSPEND, &b) < 0) die("dm resume %s", name);
  close(ctl);
  return (dev_t)b.io.dev;
}

static void node_for(dev_t dev, const char *path) {
  // the ioctl carries the kernel's encoding: minor low 8 bits, major 12 bits, minor high bits
  unsigned maj = (dev >> 8) & 0xfff, min = (dev & 0xff) | ((dev >> 12) & 0xfff00);
  unlink(path);
  if (mknod(path, S_IFBLK | 0400, makedev(maj, min)) < 0) die("node %s", path);
}

static void must_mount(const char *src, const char *dst, const char *type, unsigned long flags, const char *data) {
  mkdir(dst, 0755);
  if (mount(src, dst, type, flags, data) < 0) die("mount %s on %s", type, dst);
}

int main(void) {
  must_mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
  must_mount("sysfs", "/sys", "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
  must_mount("devtmpfs", "/dev", "devtmpfs", MS_NOSUID, "mode=0755");
  int console = open("/dev/console", O_RDWR);
  if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); if (console > 2) close(console); }
  setvbuf(stdout, NULL, _IOLBF, 0);
  printf("VMINIT start\n");
  load_modules("/module-order.txt", "");

  char *root = cmdline_value("enclave.roothash"), *geom = cmdline_value("enclave.root");
  if (!root || !hexn(root, 64) || !geom) { errno = EINVAL; die("enclave.roothash and enclave.root are required"); }
  char *colon = strchr(geom, ':');
  if (!colon) { errno = EINVAL; die("enclave.root must be <blocks>:<salt>"); }
  *colon = 0;
  uint64_t blocks = strtoull(geom, NULL, 10);
  const char *salt = colon + 1;
  if (blocks < 2 || !hexn(salt, 64)) { errno = EINVAL; die("enclave.root geometry"); }

  char data[32], hash[32]; uint64_t dsect, hsect;
  for (int i = 0; ; i++) {             // virtio disks appear a moment after the driver binds
    DIR *d = opendir("/sys/block"); int seen = 0; struct dirent *e;
    while (d && (e = readdir(d))) if (!strncmp(e->d_name, "vd", 2)) seen++;
    if (d) closedir(d);
    if (seen >= 2) break;
    if (i > 100) { errno = ENODEV; die("no virtio disks"); }
    usleep(100000);
  }
  disk_by_serial("ROOT-DATA", data, sizeof(data), &dsect);
  disk_by_serial("ROOT-HASH", hash, sizeof(hash), &hsect);
  if (dsect < blocks * 8) { errno = EINVAL; die("root disk is smaller than the measured geometry"); }
  node_for(verity_open("vroot", data, hash, blocks, 0, root, salt), "/dev/vroot");
  must_mount("/dev/vroot", "/lower", "squashfs", MS_RDONLY | MS_NODEV | MS_NOSUID, NULL);
  // setuid programs of the desktop (su, the X wrapper) live on the root, so the overlay itself allows them
  must_mount("tmpfs", "/rw", "tmpfs", MS_NODEV, "mode=0755,size=50%");
  mkdir("/rw/upper", 0755); mkdir("/rw/work", 0755);
  must_mount("overlay", "/newroot", "overlay", MS_NODEV, "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work");

  char *models = cmdline_value("enclave.models");
  if (models) {
    char *f[3] = {models, NULL, NULL}; int k = 1;
    for (char *p = models; *p && k < 3; p++) if (*p == ':') { *p = 0; f[k++] = p + 1; }
    uint64_t mblocks = strtoull(f[0], NULL, 10);
    if (k != 3 || mblocks < 2 || !hexn(f[1], 64) || !hexn(f[2], 64)) { errno = EINVAL; die("enclave.models geometry"); }
    char mdev[32]; uint64_t msect;
    disk_by_serial("MODELS", mdev, sizeof(mdev), &msect);
    if (msect < (mblocks + 1) * 8) { errno = EINVAL; die("model disk is smaller than the measured geometry"); }
    node_for(verity_open("vmodels", mdev, mdev, mblocks, mblocks + 1, f[1], f[2]), "/dev/vmodels");
    must_mount("/dev/vmodels", "/newroot/models", "ext4", MS_RDONLY | MS_NODEV | MS_NOSUID | MS_NOEXEC, "noload");
  }

  // the rest of the kernel modules come from the verified root
  load_modules("/newroot/vm/module-order.txt", "/newroot");
  if (mount("/dev", "/newroot/dev", NULL, MS_MOVE, NULL) < 0) die("move /dev");
  umount2("/proc", MNT_DETACH); umount2("/sys", MNT_DETACH);
  if (chdir("/newroot") < 0 || mount(".", "/", NULL, MS_MOVE, NULL) < 0 || chroot(".") < 0 || chdir("/") < 0) die("switch root");
  printf("VMINIT root verified, starting /vm/start\n");
  // This process stays PID 1: it reaps what the desktop orphans, and when /vm/start ends the guest powers off.
  pid_t child = fork();
  if (child < 0) die("fork");
  if (child == 0) {
    char *env[] = {"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "HOME=/root", NULL};
    char *argv[] = {"/vm/start", NULL};
    execve(argv[0], argv, env);
    printf("VMINIT FATAL: exec /vm/start (%s)\n", strerror(errno));
    _exit(127);
  }
  for (;;) {
    int status; pid_t p = wait(&status);
    if (p == child || (p < 0 && errno == ECHILD)) break;
  }
  printf("VMINIT /vm/start ended, powering off\n");
  sync(); reboot(RB_POWER_OFF);
  return 0;
}
