################################################################################
#
# xfconf
#
################################################################################

XFCONF_VERSION = 4.18.3
XFCONF_SOURCE = xfconf-$(XFCONF_VERSION).tar.bz2
XFCONF_SITE = https://archive.xfce.org/src/xfce/xfconf/$(basename $(XFCONF_VERSION))
XFCONF_LICENSE = GPL-2.0+
XFCONF_LICENSE_FILES = COPYING
XFCONF_INSTALL_STAGING = YES
XFCONF_DEPENDENCIES = libxfce4util dbus libglib2
XFCONF_CONF_OPTS = --disable-gtk-doc --enable-introspection=no

$(eval $(autotools-package))
