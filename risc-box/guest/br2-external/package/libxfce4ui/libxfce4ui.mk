################################################################################
#
# libxfce4ui
#
################################################################################

LIBXFCE4UI_VERSION = 4.18.6
LIBXFCE4UI_SOURCE = libxfce4ui-$(LIBXFCE4UI_VERSION).tar.bz2
LIBXFCE4UI_SITE = https://archive.xfce.org/src/xfce/libxfce4ui/$(basename $(LIBXFCE4UI_VERSION))
LIBXFCE4UI_LICENSE = LGPL-2.1+
LIBXFCE4UI_LICENSE_FILES = COPYING
LIBXFCE4UI_INSTALL_STAGING = YES
LIBXFCE4UI_DEPENDENCIES = libxfce4util xfconf libgtk3
LIBXFCE4UI_CONF_OPTS = --disable-gtk-doc --enable-introspection=no --disable-glibtop --disable-epoxy

$(eval $(autotools-package))
