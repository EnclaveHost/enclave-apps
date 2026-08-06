################################################################################
#
# xfwm4
#
################################################################################

XFWM4_VERSION = 4.18.0
XFWM4_SOURCE = xfwm4-$(XFWM4_VERSION).tar.bz2
XFWM4_SITE = https://archive.xfce.org/src/xfce/xfwm4/$(basename $(XFWM4_VERSION))
XFWM4_LICENSE = GPL-2.0+
XFWM4_LICENSE_FILES = COPYING
XFWM4_INSTALL_STAGING = YES
XFWM4_DEPENDENCIES = libxfce4ui libxfce4util xfconf libwnck3 libgtk3 xlib_libXinerama
XFWM4_CONF_OPTS = --disable-startup-notification --disable-xsync --disable-epoxy

$(eval $(autotools-package))
