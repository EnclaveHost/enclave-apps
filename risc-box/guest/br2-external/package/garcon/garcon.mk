################################################################################
#
# garcon
#
################################################################################

GARCON_VERSION = 4.18.2
GARCON_SOURCE = garcon-$(GARCON_VERSION).tar.bz2
GARCON_SITE = https://archive.xfce.org/src/xfce/garcon/$(basename $(GARCON_VERSION))
GARCON_LICENSE = LGPL-2.0+
GARCON_LICENSE_FILES = COPYING
GARCON_INSTALL_STAGING = YES
GARCON_DEPENDENCIES = libxfce4util libxfce4ui libgtk3
GARCON_CONF_OPTS = --disable-gtk-doc --enable-introspection=no

$(eval $(autotools-package))
