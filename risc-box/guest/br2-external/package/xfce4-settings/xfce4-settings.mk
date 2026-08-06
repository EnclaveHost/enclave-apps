################################################################################
#
# xfce4-settings
#
################################################################################

XFCE4_SETTINGS_VERSION = 4.18.6
XFCE4_SETTINGS_SOURCE = xfce4-settings-$(XFCE4_SETTINGS_VERSION).tar.bz2
XFCE4_SETTINGS_SITE = https://archive.xfce.org/src/xfce/xfce4-settings/$(basename $(XFCE4_SETTINGS_VERSION))
XFCE4_SETTINGS_LICENSE = GPL-2.0+
XFCE4_SETTINGS_LICENSE_FILES = COPYING
XFCE4_SETTINGS_INSTALL_STAGING = YES
XFCE4_SETTINGS_DEPENDENCIES = libxfce4ui libxfce4util xfconf garcon exo libgtk3 fontconfig xlib_libX11 xlib_libXi
XFCE4_SETTINGS_CONF_OPTS = --disable-libnotify --disable-upower-glib --disable-xorg-libinput --disable-libxklavier --disable-colord

$(eval $(autotools-package))
