################################################################################
#
# xfce4-panel
#
################################################################################

XFCE4_PANEL_VERSION = 4.18.6
XFCE4_PANEL_SOURCE = xfce4-panel-$(XFCE4_PANEL_VERSION).tar.bz2
XFCE4_PANEL_SITE = https://archive.xfce.org/src/xfce/xfce4-panel/$(basename $(XFCE4_PANEL_VERSION))
XFCE4_PANEL_LICENSE = GPL-2.0+, LGPL-2.1+
XFCE4_PANEL_LICENSE_FILES = COPYING
XFCE4_PANEL_INSTALL_STAGING = YES
XFCE4_PANEL_DEPENDENCIES = libxfce4ui libxfce4util garcon exo libwnck3 libgtk3
XFCE4_PANEL_CONF_OPTS = --disable-gtk-doc --enable-introspection=no

$(eval $(autotools-package))
