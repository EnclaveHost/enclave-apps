################################################################################
#
# libxfce4util
#
################################################################################

LIBXFCE4UTIL_VERSION = 4.18.2
LIBXFCE4UTIL_SOURCE = libxfce4util-$(LIBXFCE4UTIL_VERSION).tar.bz2
LIBXFCE4UTIL_SITE = https://archive.xfce.org/src/xfce/libxfce4util/$(basename $(LIBXFCE4UTIL_VERSION))
LIBXFCE4UTIL_LICENSE = LGPL-2.1+
LIBXFCE4UTIL_LICENSE_FILES = COPYING
LIBXFCE4UTIL_INSTALL_STAGING = YES
LIBXFCE4UTIL_DEPENDENCIES = libglib2
LIBXFCE4UTIL_CONF_OPTS = --disable-gtk-doc --enable-introspection=no

$(eval $(autotools-package))
