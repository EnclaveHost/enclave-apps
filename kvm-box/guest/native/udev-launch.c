/* Only the measured native nsinit launches this helper, after filesystem setup.
 * It takes no tenant command, endpoint, rules path or environment overrides.
 */
#define _GNU_SOURCE
#include "udev-policy.h"
#include <stdlib.h>

int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1 || getuid() || geteuid()) return 64;
    if (syscall(SYS_close_range, 3U, ~0U, 0) || clearenv()) return 65;
    if (udev_drop_capabilities() || udev_install_seccomp()) return 66;
    char *const args[] = { "/usr/lib/systemd/systemd-udevd", "--resolve-names=never",
                          "--children-max=4", NULL };
    char *const env[] = { "PATH=/usr/bin:/bin", "SYSTEMD_LOG_LEVEL=err", NULL };
    execve(args[0], args, env);
    return 127;
}
