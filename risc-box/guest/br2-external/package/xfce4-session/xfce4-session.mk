################################################################################
#
# xfce4-session
#
################################################################################

XFCE4_SESSION_VERSION = 4.18.4
XFCE4_SESSION_SOURCE = xfce4-session-$(XFCE4_SESSION_VERSION).tar.bz2
XFCE4_SESSION_SITE = https://archive.xfce.org/src/xfce/xfce4-session/$(basename $(XFCE4_SESSION_VERSION))
XFCE4_SESSION_LICENSE = GPL-2.0+
XFCE4_SESSION_LICENSE_FILES = COPYING
XFCE4_SESSION_INSTALL_STAGING = YES
XFCE4_SESSION_DEPENDENCIES = libxfce4ui libxfce4util xfconf libwnck3 libgtk3 xapp_iceauth
XFCE4_SESSION_CONF_OPTS = --disable-legacy-sm --disable-polkit

# configure locates iceauth with AC_PATH_PROG, which searches the BUILD
# machine's PATH and errors out when it finds nothing — installing iceauth for
# the target does not help it. Tell it the path the binary will have on the
# guest; xapp_iceauth (a dependency above) is what puts it there.
XFCE4_SESSION_CONF_ENV = ac_cv_path_ICEAUTH=/usr/bin/iceauth

$(eval $(autotools-package))
